//! ESM module bundling and execution support.
//!
//! This module transforms ES modules into a bundled format that can be
//! executed in JSC without native module support.
//!
//! # How it works
//!
//! Each module is wrapped in a factory function that registers its exports:
//! ```javascript
//! __otter_modules["./foo.js"] = (function() {
//!     const exports = {};
//!     // Original module code with imports transformed
//!     exports.default = foo;
//!     exports.bar = bar;
//!     return exports;
//! })();
//! ```
//!
//! Import statements are transformed to read from the registry:
//! ```javascript
//! // Before: import { foo } from './foo.js';
//! // After:  const { foo } = __otter_modules["./foo.js"];
//! ```

use regex::{Captures, Regex};
use std::collections::HashMap;

use crate::modules_ast::transform_module_ast;

fn node_builtin_expr(resolved: &str) -> Option<String> {
    let name = resolved.strip_prefix("node:")?;
    Some(format!("globalThis.__otter_node_builtins[\"{}\"]", name))
}

fn otter_builtin_expr(resolved: &str) -> Option<String> {
    let name = resolved.strip_prefix("otter:")?;
    Some(format!("globalThis.__otter_node_builtins[\"{}\"]", name))
}

/// Check if resolved URL is any built-in (node: or otter:) and return the expr
fn builtin_expr(resolved: &str) -> Option<String> {
    node_builtin_expr(resolved).or_else(|| otter_builtin_expr(resolved))
}

/// Transform import/export statements in module source.
///
/// Converts ESM syntax to use the `__otter_modules` registry.
/// Uses SWC AST-based transformation for correct handling of all ESM patterns.
pub fn transform_module(
    source: &str,
    module_url: &str,
    dependencies: &HashMap<String, String>,
) -> String {
    // Use AST-based transform (handles all ESM patterns correctly)
    match transform_module_ast(source, module_url, dependencies) {
        Ok(transformed) => return transformed,
        Err(_) => {
            // Fallback to regex for non-parseable content (e.g., already-transformed code)
        }
    }

    // Regex fallback for edge cases
    let mut result = source.to_string();

    // Transform static imports
    // import { foo, bar } from './mod.js' -> const { foo, bar } = __otter_modules["resolved_url"];
    // import foo from './mod.js' -> const foo = __otter_modules["resolved_url"].default;
    // import * as mod from './mod.js' -> const mod = __otter_modules["resolved_url"];
    // import './side-effect.js' -> __otter_modules["resolved_url"];

    let import_default_re =
        Regex::new(r#"(?m)^(\s*)import\s+(\w+)\s+from\s+['"]([^'"]+)['"];\s*$"#).unwrap();

    let import_named_re =
        Regex::new(r#"(?m)^(\s*)import\s+\{([^}]+)\}\s+from\s+['"]([^'"]+)['"];\s*$"#).unwrap();

    let import_namespace_re =
        Regex::new(r#"(?m)^(\s*)import\s+\*\s+as\s+(\w+)\s+from\s+['"]([^'"]+)['"];\s*$"#).unwrap();

    let import_side_effect_re = Regex::new(r#"(?m)^(\s*)import\s+['"]([^'"]+)['"];\s*$"#).unwrap();

    let import_default_named_re =
        Regex::new(r#"(?m)^(\s*)import\s+(\w+)\s*,\s*\{([^}]+)\}\s+from\s+['"]([^'"]+)['"];\s*$"#)
            .unwrap();

    // Helper to resolve specifier
    let resolve = |specifier: &str| -> String {
        dependencies
            .get(specifier)
            .cloned()
            .unwrap_or_else(|| specifier.to_string())
    };

    // Order matters - more specific patterns first

    // import foo, { bar } from './mod.js'
    result = import_default_named_re.replace_all(&result, |caps: &Captures| {
        let indent = &caps[1];
        let default_name = &caps[2];
        let named = &caps[3];
        let specifier = &caps[4];
        let resolved = resolve(specifier);
        if let Some(expr) = builtin_expr(&resolved) {
            return format!(
                "{}const {} = {};\n{}const {{{}}} = {};",
                indent, default_name, expr, indent, named, expr
            );
        }
        format!(
            "{}const {} = __otter_modules[\"{}\"].default;\n{}const {{{}}} = __otter_modules[\"{}\"];",
            indent, default_name, resolved, indent, named, resolved
        )
    }).to_string();

    // import foo from './mod.js'
    result = import_default_re
        .replace_all(&result, |caps: &Captures| {
            let indent = &caps[1];
            let name = &caps[2];
            let specifier = &caps[3];
            let resolved = resolve(specifier);
            if let Some(expr) = builtin_expr(&resolved) {
                return format!("{}const {} = {};", indent, name, expr);
            }
            format!(
                "{}const {} = __otter_modules[\"{}\"].default;",
                indent, name, resolved
            )
        })
        .to_string();

    // import { foo, bar } from './mod.js'
    result = import_named_re
        .replace_all(&result, |caps: &Captures| {
            let indent = &caps[1];
            let names = &caps[2];
            let specifier = &caps[3];
            let resolved = resolve(specifier);
            if let Some(expr) = builtin_expr(&resolved) {
                return format!("{}const {{{}}} = {};", indent, names, expr);
            }
            format!(
                "{}const {{{}}} = __otter_modules[\"{}\"];",
                indent, names, resolved
            )
        })
        .to_string();

    // import * as mod from './mod.js'
    result = import_namespace_re
        .replace_all(&result, |caps: &Captures| {
            let indent = &caps[1];
            let name = &caps[2];
            let specifier = &caps[3];
            let resolved = resolve(specifier);
            if let Some(expr) = builtin_expr(&resolved) {
                return format!("{}const {} = {};", indent, name, expr);
            }
            format!(
                "{}const {} = __otter_modules[\"{}\"];",
                indent, name, resolved
            )
        })
        .to_string();

    // import './side-effect.js'
    result = import_side_effect_re
        .replace_all(&result, |caps: &Captures| {
            let indent = &caps[1];
            let specifier = &caps[2];
            let resolved = resolve(specifier);
            if let Some(expr) = builtin_expr(&resolved) {
                return format!("{}{};", indent, expr);
            }
            format!("{}__otter_modules[\"{}\"];", indent, resolved)
        })
        .to_string();

    // Transform dynamic imports: import('./foo.js') -> Promise.resolve(__otter_modules["resolved_url"])
    // This handles string literal specifiers only - truly dynamic specifiers need runtime loading
    let dynamic_import_re = Regex::new(r#"import\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap();

    result = dynamic_import_re
        .replace_all(&result, |caps: &Captures| {
            let specifier = &caps[1];
            let resolved = resolve(specifier);
            if let Some(expr) = builtin_expr(&resolved) {
                return format!("Promise.resolve({})", expr);
            }
            format!("Promise.resolve(__otter_modules[\"{}\"])", resolved)
        })
        .to_string();

    // Transform export statements
    // export const foo = 1; -> const foo = 1; __otter_exports.foo = foo;
    // export default foo; -> __otter_exports.default = foo;
    // export { foo, bar }; -> __otter_exports.foo = foo; __otter_exports.bar = bar;

    let export_default_re = Regex::new(r#"(?m)^(\s*)export\s+default\s+(.+?);?\s*$"#).unwrap();

    let export_const_re = Regex::new(r#"(?m)^(\s*)export\s+(const|let|var)\s+(\w+)\s*="#).unwrap();

    let export_function_re =
        Regex::new(r#"(?m)^(\s*)export\s+(async\s+)?function\s+(\w+)"#).unwrap();

    let export_class_re = Regex::new(r#"(?m)^(\s*)export\s+class\s+(\w+)"#).unwrap();

    let export_named_re = Regex::new(r#"(?m)^(\s*)export\s+\{([^}]+)\};\s*$"#).unwrap();

    // Transform re-exports: export { foo } from './mod.js'
    let reexport_re =
        Regex::new(r#"(?m)^(\s*)export\s+\{([^}]+)\}\s+from\s+['"]([^'"]+)['"];\s*$"#).unwrap();

    // Transform re-export all: export * from './mod.js'
    let reexport_all_re =
        Regex::new(r#"(?m)^(\s*)export\s+\*\s+from\s+['"]([^'"]+)['"];\s*$"#).unwrap();

    // Collect exported names for post-processing
    let mut exported_names: Vec<String> = Vec::new();

    // export default expression
    result = export_default_re
        .replace_all(&result, |caps: &Captures| {
            let indent = &caps[1];
            let expr = caps[2].trim_end_matches(';');
            format!("{}__otter_exports.default = {};", indent, expr)
        })
        .to_string();

    // export const/let/var
    for cap in export_const_re.captures_iter(&result.clone()) {
        exported_names.push(cap[3].to_string());
    }
    result = export_const_re
        .replace_all(&result, |caps: &Captures| {
            format!("{}{} {} =", &caps[1], &caps[2], &caps[3])
        })
        .to_string();

    // export function
    for cap in export_function_re.captures_iter(&result.clone()) {
        exported_names.push(cap[3].to_string());
    }
    result = export_function_re
        .replace_all(&result, |caps: &Captures| {
            let async_kw = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            format!("{}{}function {}", &caps[1], async_kw, &caps[3])
        })
        .to_string();

    // export class
    for cap in export_class_re.captures_iter(&result.clone()) {
        exported_names.push(cap[2].to_string());
    }
    result = export_class_re
        .replace_all(&result, |caps: &Captures| {
            format!("{}class {}", &caps[1], &caps[2])
        })
        .to_string();

    // export { foo, bar }
    result = export_named_re
        .replace_all(&result, |caps: &Captures| {
            let indent = &caps[1];
            let names: Vec<&str> = caps[2]
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty()) // Filter out empty names (handles `export { }`)
                .collect();
            if names.is_empty() {
                // Empty export (e.g., `export { }`) - just remove it
                return String::new();
            }
            names
                .iter()
                .map(|name| {
                    // Handle aliased exports: export { foo as bar }
                    if let Some((original, alias)) = name.split_once(" as ") {
                        format!(
                            "{}__otter_exports.{} = {};",
                            indent,
                            alias.trim(),
                            original.trim()
                        )
                    } else {
                        format!("{}__otter_exports.{} = {};", indent, name, name)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .to_string();

    // export { foo } from './mod.js'
    result = reexport_re
        .replace_all(&result, |caps: &Captures| {
            let indent = &caps[1];
            let names: Vec<&str> = caps[2].split(',').map(|s| s.trim()).collect();
            let specifier = &caps[3];
            let resolved = resolve(specifier);
            let module_expr = builtin_expr(&resolved)
                .unwrap_or_else(|| format!("__otter_modules[\"{}\"]", resolved));
            names
                .iter()
                .map(|name| {
                    if let Some((original, alias)) = name.split_once(" as ") {
                        format!(
                            "{}__otter_exports.{} = {}.{};",
                            indent,
                            alias.trim(),
                            module_expr,
                            original.trim()
                        )
                    } else {
                        format!(
                            "{}__otter_exports.{} = {}.{};",
                            indent, name, module_expr, name
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .to_string();

    // export * from './mod.js'
    result = reexport_all_re
        .replace_all(&result, |caps: &Captures| {
            let indent = &caps[1];
            let specifier = &caps[2];
            let resolved = resolve(specifier);
            let module_expr = builtin_expr(&resolved)
                .unwrap_or_else(|| format!("__otter_modules[\"{}\"]", resolved));
            format!(
                "{}Object.assign(__otter_exports, {} || {{}});",
                indent, module_expr
            )
        })
        .to_string();

    // Add export assignments for collected names at the end
    if !exported_names.is_empty() {
        result.push_str("\n// Export assignments\n");
        for name in exported_names {
            result.push_str(&format!("__otter_exports.{} = {};\n", name, name));
        }
    }

    // Add source URL comment for debugging
    result.push_str(&format!("\n//# sourceURL={}\n", module_url));

    result
}

/// Wrap a module in a sync factory function (for tests and simple cases).
pub fn wrap_module(url: &str, transformed_source: &str) -> String {
    format!(
        r#"__otter_modules["{}"] = (function() {{
    const __otter_exports = {{}};
    {}
    return __otter_exports;
}})();
"#,
        url, transformed_source
    )
}

/// Wrap a module in an async factory function.
/// Uses async IIFE to support top-level await in modules.
fn wrap_module_async(url: &str, transformed_source: &str) -> String {
    format!(
        r#"__otter_modules["{}"] = await (async function() {{
    const __otter_exports = {{}};
    {}
    return __otter_exports;
}})();
"#,
        url, transformed_source
    )
}

/// Bundle multiple modules into a single executable script.
///
/// Modules must be provided in topological order (dependencies first).
/// The entire bundle is wrapped in an async IIFE to support top-level await.
pub fn bundle_modules(modules: Vec<(&str, &str, &HashMap<String, String>)>) -> String {
    let mut result = String::new();

    // Wrap everything in an async IIFE to support top-level await
    result.push_str("(async () => {\n");

    // Initialize the module registry
    result.push_str("globalThis.__otter_modules = globalThis.__otter_modules || {};\n\n");

    // Add each module in order (await ensures sequential loading)
    for (url, source, deps) in modules {
        let transformed = transform_module(source, url, deps);
        let wrapped = wrap_module_async(url, &transformed);
        result.push_str(&wrapped);
        result.push('\n');
    }

    result.push_str("})();\n");

    result
}

/// Module format for bundling
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleFormat {
    /// ES Modules (import/export)
    ESM,
    /// CommonJS (require/module.exports)
    CommonJS,
}

/// Information about a module for bundling
pub struct ModuleInfo<'a> {
    pub url: &'a str,
    pub source: &'a str,
    pub dependencies: &'a HashMap<String, String>,
    pub format: ModuleFormat,
    pub dirname: Option<&'a str>,
    pub filename: Option<&'a str>,
}

/// Bundle multiple modules with mixed formats (ESM and CommonJS).
///
/// Modules must be provided in topological order (dependencies first).
/// This function handles interoperability between ESM and CommonJS modules.
/// The entire bundle is wrapped in an async IIFE to support top-level await.
pub fn bundle_modules_mixed(modules: Vec<ModuleInfo<'_>>) -> String {
    let mut result = String::new();

    // NOTE: This function returns raw code without an async IIFE wrapper.
    // The caller (run.rs) wraps everything in `(async () => { try { ... } catch { ... } })();`
    // which provides the async context for top-level await support.

    // Initialize both module registries
    result.push_str("globalThis.__otter_modules = globalThis.__otter_modules || {};\n");
    result.push_str("globalThis.__otter_cjs_modules = globalThis.__otter_cjs_modules || {};\n\n");

    // Add each module in order
    for module in modules {
        match module.format {
            ModuleFormat::ESM => {
                let transformed = transform_module(module.source, module.url, module.dependencies);
                let wrapped = wrap_module_async(module.url, &transformed);
                result.push_str(&wrapped);
            }
            ModuleFormat::CommonJS => {
                let dirname = module.dirname.unwrap_or("");
                let filename = module.filename.unwrap_or(module.url);
                let wrapped = crate::commonjs::wrap_commonjs_module(
                    module.url,
                    module.source,
                    dirname,
                    filename,
                    module.dependencies,
                );
                result.push_str(&wrapped);

                // Register in ESM registry with lazy __toESM wrapper for interop
                // Uses a getter that evaluates the CJS module on first access,
                // then replaces itself with the cached value
                result.push_str(&format!(
                    r#"Object.defineProperty(__otter_modules, "{0}", {{
  get: function() {{
    var mod = __toESM(__otter_cjs_modules["{0}"](), 1);
    Object.defineProperty(__otter_modules, "{0}", {{ value: mod, writable: true, configurable: true }});
    return mod;
  }},
  configurable: true,
  enumerable: true
}});
"#,
                    module.url
                ));
            }
        }
        result.push('\n');
    }

    result
}

/// Generate code to execute the entry module (supports both ESM and CJS).
pub fn entry_execution_mixed(entry_url: &str, format: ModuleFormat) -> String {
    match format {
        ModuleFormat::ESM => entry_execution(entry_url),
        ModuleFormat::CommonJS => format!(
            r#"// Execute CommonJS entry module
(function() {{
    var mod = globalThis.__otter_cjs_modules["{}"];
    if (mod) {{
        mod();
    }} else {{
        throw new Error("Entry module not found: {}");
    }}
}})();
"#,
            entry_url, entry_url
        ),
    }
}

/// Generate code to execute the entry module.
pub fn entry_execution(entry_url: &str) -> String {
    format!(
        r#"// Execute entry module
(function() {{
    const entry = __otter_modules["{}"];
    if (entry && typeof entry.default === 'function') {{
        entry.default();
    }}
}})();
"#,
        entry_url
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transform_import_default() {
        let source = "import foo from './foo.js';";
        let mut deps = HashMap::new();
        deps.insert("./foo.js".to_string(), "file:///project/foo.js".to_string());

        let result = transform_module(source, "file:///project/main.js", &deps);
        assert!(
            result.contains(r#"const foo = __otter_modules["file:///project/foo.js"].default;"#)
        );
    }

    #[test]
    fn test_transform_import_named() {
        let source = "import { foo, bar } from './mod.js';";
        let mut deps = HashMap::new();
        deps.insert("./mod.js".to_string(), "file:///project/mod.js".to_string());

        let result = transform_module(source, "file:///project/main.js", &deps);
        // AST produces individual const statements for each named import
        assert!(result.contains(r#"__otter_modules["file:///project/mod.js"].foo"#));
        assert!(result.contains(r#"__otter_modules["file:///project/mod.js"].bar"#));
    }

    #[test]
    fn test_transform_import_namespace() {
        let source = "import * as utils from './utils.js';";
        let mut deps = HashMap::new();
        deps.insert(
            "./utils.js".to_string(),
            "file:///project/utils.js".to_string(),
        );

        let result = transform_module(source, "file:///project/main.js", &deps);
        assert!(result.contains(r#"const utils = __otter_modules["file:///project/utils.js"];"#));
    }

    #[test]
    fn test_transform_export_default() {
        let source = "export default function foo() {}";
        let deps = HashMap::new();

        let result = transform_module(source, "file:///project/foo.js", &deps);
        // AST converts export default function to named function + export
        assert!(result.contains("function foo()"));
        assert!(result.contains("__otter_exports.default = foo"));
    }

    #[test]
    fn test_transform_export_const() {
        let source = "export const PI = 3.14;";
        let deps = HashMap::new();

        let result = transform_module(source, "file:///project/math.js", &deps);
        assert!(result.contains("const PI ="));
        assert!(result.contains("__otter_exports.PI = PI;"));
    }

    #[test]
    fn test_wrap_module() {
        let source = "const foo = 1;\n__otter_exports.default = foo;";
        let wrapped = wrap_module("file:///test.js", source);

        assert!(wrapped.contains(r#"__otter_modules["file:///test.js"]"#));
        assert!(wrapped.contains("const __otter_exports = {};"));
        assert!(wrapped.contains("return __otter_exports;"));
    }

    #[test]
    fn test_bundle_modules() {
        let mut deps = HashMap::new();
        deps.insert("./dep.js".to_string(), "file:///dep.js".to_string());
        let empty_deps = HashMap::new();

        let modules = vec![
            ("file:///dep.js", "export const x = 1;", &empty_deps),
            (
                "file:///main.js",
                "import { x } from './dep.js';\nconsole.log(x);",
                &deps,
            ),
        ];

        let bundle = bundle_modules(modules);
        assert!(bundle.contains("globalThis.__otter_modules"));
        assert!(bundle.contains(r#"__otter_modules["file:///dep.js"]"#));
        assert!(bundle.contains(r#"__otter_modules["file:///main.js"]"#));
    }

    #[test]
    fn test_transform_dynamic_import() {
        let source = "const mod = await import('./dynamic.js');";
        let mut deps = HashMap::new();
        deps.insert(
            "./dynamic.js".to_string(),
            "file:///project/dynamic.js".to_string(),
        );

        let result = transform_module(source, "file:///project/main.js", &deps);
        // AST transforms dynamic import to Promise.resolve
        assert!(result.contains(r#"Promise.resolve"#));
        assert!(result.contains(r#"__otter_modules["file:///project/dynamic.js"]"#));
    }

    #[test]
    fn test_transform_import_side_effect() {
        let source = "import './polyfill.js';";
        let mut deps = HashMap::new();
        deps.insert(
            "./polyfill.js".to_string(),
            "file:///project/polyfill.js".to_string(),
        );

        let result = transform_module(source, "file:///project/main.js", &deps);
        assert!(result.contains(r#"__otter_modules["file:///project/polyfill.js"];"#));
    }

    #[test]
    fn test_transform_import_default_and_named() {
        let source = "import React, { useState } from 'react';";
        let mut deps = HashMap::new();
        deps.insert(
            "react".to_string(),
            "file:///node_modules/react/index.js".to_string(),
        );

        let result = transform_module(source, "file:///project/main.js", &deps);
        // AST produces individual const for default and each named import
        assert!(result.contains(r#"__otter_modules["file:///node_modules/react/index.js"].default"#));
        assert!(result.contains(r#"__otter_modules["file:///node_modules/react/index.js"].useState"#));
    }

    #[test]
    fn test_transform_node_builtin_import_named() {
        let source = "import { format } from 'node:util';";
        let mut deps = HashMap::new();
        deps.insert("node:util".to_string(), "node:util".to_string());

        let result = transform_module(source, "file:///project/main.js", &deps);
        // AST produces individual const for each named import from builtin
        assert!(result.contains(r#"globalThis.__otter_node_builtins["util"].format"#));
    }

    #[test]
    fn test_transform_node_builtin_dynamic_import() {
        let source = "const mod = await import('node:util');";
        let mut deps = HashMap::new();
        deps.insert("node:util".to_string(), "node:util".to_string());

        let result = transform_module(source, "file:///project/main.js", &deps);
        // AST transforms dynamic import to Promise.resolve
        assert!(result.contains(r#"Promise.resolve"#));
        assert!(result.contains(r#"globalThis.__otter_node_builtins["util"]"#));
    }

    #[test]
    fn test_transform_export_function() {
        let source = "export function greet(name) { return `Hello, ${name}`; }";
        let deps = HashMap::new();

        let result = transform_module(source, "file:///project/utils.js", &deps);
        assert!(result.contains("function greet(name)"));
        assert!(result.contains("__otter_exports.greet = greet;"));
    }

    #[test]
    fn test_transform_reexport() {
        let source = "export { foo, bar as baz } from './other.js';";
        let mut deps = HashMap::new();
        deps.insert(
            "./other.js".to_string(),
            "file:///project/other.js".to_string(),
        );

        let result = transform_module(source, "file:///project/index.js", &deps);
        assert!(
            result.contains(
                r#"__otter_exports.foo = __otter_modules["file:///project/other.js"].foo;"#
            )
        );
        assert!(
            result.contains(
                r#"__otter_exports.baz = __otter_modules["file:///project/other.js"].bar;"#
            )
        );
    }

    #[test]
    fn test_transform_reexport_all() {
        let source = "export * from './utils.js';";
        let mut deps = HashMap::new();
        deps.insert(
            "./utils.js".to_string(),
            "file:///project/utils.js".to_string(),
        );

        let result = transform_module(source, "file:///project/index.js", &deps);
        assert!(result.contains(
            r#"Object.assign(__otter_exports, __otter_modules["file:///project/utils.js"]"#
        ));
    }

    #[test]
    fn test_bundle_modules_mixed_esm_only() {
        let empty_deps = HashMap::new();

        let modules = vec![ModuleInfo {
            url: "file:///project/main.js",
            source: "export const x = 1;",
            dependencies: &empty_deps,
            format: ModuleFormat::ESM,
            dirname: None,
            filename: None,
        }];

        let bundle = bundle_modules_mixed(modules);
        assert!(bundle.contains("globalThis.__otter_modules"));
        assert!(bundle.contains("globalThis.__otter_cjs_modules"));
        assert!(bundle.contains(r#"__otter_modules["file:///project/main.js"]"#));
    }

    #[test]
    fn test_bundle_modules_mixed_cjs_only() {
        let empty_deps = HashMap::new();

        let modules = vec![ModuleInfo {
            url: "file:///project/lib.cjs",
            source: "module.exports = { foo: 1 };",
            dependencies: &empty_deps,
            format: ModuleFormat::CommonJS,
            dirname: Some("/project"),
            filename: Some("/project/lib.cjs"),
        }];

        let bundle = bundle_modules_mixed(modules);
        assert!(bundle.contains("__otter_cjs_modules[\"file:///project/lib.cjs\"]"));
        // Should also register in ESM registry for interop (via lazy getter)
        assert!(bundle.contains("Object.defineProperty(__otter_modules, \"file:///project/lib.cjs\""));
        assert!(bundle.contains("__toESM"));
    }

    #[test]
    fn test_bundle_modules_mixed_interop() {
        let mut esm_deps = HashMap::new();
        esm_deps.insert(
            "./lib.cjs".to_string(),
            "file:///project/lib.cjs".to_string(),
        );
        let empty_deps = HashMap::new();

        let modules = vec![
            ModuleInfo {
                url: "file:///project/lib.cjs",
                source: "module.exports = { helper: () => 42 };",
                dependencies: &empty_deps,
                format: ModuleFormat::CommonJS,
                dirname: Some("/project"),
                filename: Some("/project/lib.cjs"),
            },
            ModuleInfo {
                url: "file:///project/main.js",
                source: "import lib from './lib.cjs';\nconsole.log(lib.helper());",
                dependencies: &esm_deps,
                format: ModuleFormat::ESM,
                dirname: None,
                filename: None,
            },
        ];

        let bundle = bundle_modules_mixed(modules);
        // CJS module registered
        assert!(bundle.contains("__otter_cjs_modules[\"file:///project/lib.cjs\"]"));
        // ESM interop wrapper with lazy getter
        assert!(bundle.contains("Object.defineProperty(__otter_modules, \"file:///project/lib.cjs\""));
        assert!(bundle.contains("__toESM(__otter_cjs_modules[\"file:///project/lib.cjs\"](), 1)"));
        // ESM module using the CJS import
        assert!(
            bundle.contains(r#"const lib = __otter_modules["file:///project/lib.cjs"].default;"#)
        );
    }

    #[test]
    fn test_entry_execution_mixed_esm() {
        let code = entry_execution_mixed("file:///main.js", ModuleFormat::ESM);
        assert!(code.contains("__otter_modules[\"file:///main.js\"]"));
    }

    #[test]
    fn test_entry_execution_mixed_cjs() {
        let code = entry_execution_mixed("file:///main.cjs", ModuleFormat::CommonJS);
        assert!(code.contains("__otter_cjs_modules[\"file:///main.cjs\"]"));
    }
}
