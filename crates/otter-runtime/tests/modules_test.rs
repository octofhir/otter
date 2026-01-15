//! Integration tests for module bundling and execution

use otter_runtime::{JscConfig, JscRuntime, bundle_modules, transform_module, wrap_module};
use std::collections::HashMap;

/// Test that a bundled module can execute in the runtime
#[test]
fn test_bundle_executes_in_runtime() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    // Create a simple module with an export
    let deps = HashMap::new();
    let module_source = r#"
        export const greeting = "Hello, World!";
        export function add(a, b) { return a + b; }
    "#;

    let transformed = transform_module(module_source, "file:///test/mod.js", &deps);
    let wrapped = wrap_module("file:///test/mod.js", &transformed);

    // Bundle includes module registry init
    let bundle = format!(
        "globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{}",
        wrapped
    );

    runtime.eval(&bundle).unwrap();

    // Verify the module is registered and exports are accessible
    let result = runtime
        .eval(r#"__otter_modules["file:///test/mod.js"].greeting"#)
        .unwrap();
    assert_eq!(result.to_string().unwrap(), "Hello, World!");

    let result = runtime
        .eval(r#"__otter_modules["file:///test/mod.js"].add(2, 3)"#)
        .unwrap();
    assert_eq!(result.to_number().unwrap(), 5.0);
}

/// Test multi-module bundle with dependencies
#[test]
fn test_multi_module_bundle() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    // Module A: base module with no deps
    let empty_deps = HashMap::new();
    let mod_a_source = r#"
        export const PI = 3.14159;
        export function square(x) { return x * x; }
    "#;

    // Module B: depends on module A
    let mut mod_b_deps = HashMap::new();
    mod_b_deps.insert("./math.js".to_string(), "file:///test/math.js".to_string());
    let mod_b_source = r#"
        import { PI, square } from './math.js';
        export function circleArea(r) { return PI * square(r); }
    "#;

    // Bundle in dependency order
    let modules = vec![
        ("file:///test/math.js", mod_a_source, &empty_deps),
        ("file:///test/shapes.js", mod_b_source, &mod_b_deps),
    ];

    let bundle = bundle_modules(modules);
    runtime.eval(&bundle).unwrap();

    // Verify module B can use module A's exports
    let result = runtime
        .eval(r#"__otter_modules["file:///test/shapes.js"].circleArea(2)"#)
        .unwrap();

    // PI * 2^2 = 3.14159 * 4 = 12.56636
    let area = result.to_number().unwrap();
    assert!((area - 12.56636).abs() < 0.0001);
}

/// Test default export handling
#[test]
fn test_default_export() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    let deps = HashMap::new();
    let source = r#"
        export default function greet(name) {
            return "Hello, " + name + "!";
        }
    "#;

    let transformed = transform_module(source, "file:///test/greet.js", &deps);
    let wrapped = wrap_module("file:///test/greet.js", &transformed);

    let bundle = format!(
        "globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{}",
        wrapped
    );

    runtime.eval(&bundle).unwrap();

    let result = runtime
        .eval(r#"__otter_modules["file:///test/greet.js"].default("World")"#)
        .unwrap();
    assert_eq!(result.to_string().unwrap(), "Hello, World!");
}

/// Test class exports
#[test]
fn test_class_export() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    let deps = HashMap::new();
    let source = r#"
        export class Calculator {
            constructor(initial) {
                this.value = initial || 0;
            }
            add(n) {
                this.value += n;
                return this;
            }
            result() {
                return this.value;
            }
        }
    "#;

    let transformed = transform_module(source, "file:///test/calc.js", &deps);
    let wrapped = wrap_module("file:///test/calc.js", &transformed);

    let bundle = format!(
        "globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{}",
        wrapped
    );

    runtime.eval(&bundle).unwrap();

    let result = runtime
        .eval(
            r#"
            const Calculator = __otter_modules["file:///test/calc.js"].Calculator;
            new Calculator(10).add(5).add(3).result()
        "#,
        )
        .unwrap();
    assert_eq!(result.to_number().unwrap(), 18.0);
}

/// Test namespace import simulation
#[test]
fn test_namespace_import() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    // Module with multiple exports
    let empty_deps = HashMap::new();
    let utils_source = r#"
        export const VERSION = "1.0.0";
        export function upper(s) { return s.toUpperCase(); }
        export function lower(s) { return s.toLowerCase(); }
    "#;

    // Module that uses namespace import
    let mut main_deps = HashMap::new();
    main_deps.insert(
        "./utils.js".to_string(),
        "file:///test/utils.js".to_string(),
    );
    let main_source = r#"
        import * as utils from './utils.js';
        export const version = utils.VERSION;
        export const result = utils.upper("hello");
    "#;

    let modules = vec![
        ("file:///test/utils.js", utils_source, &empty_deps),
        ("file:///test/main.js", main_source, &main_deps),
    ];

    let bundle = bundle_modules(modules);
    runtime.eval(&bundle).unwrap();

    let version = runtime
        .eval(r#"__otter_modules["file:///test/main.js"].version"#)
        .unwrap();
    assert_eq!(version.to_string().unwrap(), "1.0.0");

    let result = runtime
        .eval(r#"__otter_modules["file:///test/main.js"].result"#)
        .unwrap();
    assert_eq!(result.to_string().unwrap(), "HELLO");
}

/// Test dynamic import transformation executes correctly
#[test]
fn test_dynamic_import_execution() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    // First register a module
    let deps = HashMap::new();
    let source = r#"
        export const value = 42;
    "#;

    let transformed = transform_module(source, "file:///test/dynamic.js", &deps);
    let wrapped = wrap_module("file:///test/dynamic.js", &transformed);

    let bundle = format!(
        "globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{}",
        wrapped
    );
    runtime.eval(&bundle).unwrap();

    // Now test that the transformed dynamic import works
    // Dynamic import('./dynamic.js') becomes Promise.resolve(__otter_modules["..."])
    // We can verify the module is accessible via promise resolution
    runtime
        .eval(
            r#"
            Promise.resolve(__otter_modules["file:///test/dynamic.js"])
                .then(mod => { globalThis.__dynamicResult = mod.value; });
        "#,
        )
        .unwrap();

    // Run event loop to resolve promise
    runtime
        .run_event_loop_until_idle(std::time::Duration::from_millis(100))
        .unwrap();

    // The promise should have resolved
    let result = runtime.eval("globalThis.__dynamicResult").unwrap();
    assert_eq!(result.to_number().unwrap(), 42.0);
}

/// Test re-export functionality
#[test]
fn test_reexport() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    // Original module with exports
    let empty_deps = HashMap::new();
    let base_source = r#"
        export const foo = "FOO";
        export const bar = "BAR";
        export const baz = "BAZ";
    "#;

    // Re-export module
    let mut reexport_deps = HashMap::new();
    reexport_deps.insert("./base.js".to_string(), "file:///test/base.js".to_string());
    let reexport_source = r#"
        export { foo, bar as renamedBar } from './base.js';
    "#;

    let modules = vec![
        ("file:///test/base.js", base_source, &empty_deps),
        ("file:///test/reexport.js", reexport_source, &reexport_deps),
    ];

    let bundle = bundle_modules(modules);
    runtime.eval(&bundle).unwrap();

    let foo = runtime
        .eval(r#"__otter_modules["file:///test/reexport.js"].foo"#)
        .unwrap();
    assert_eq!(foo.to_string().unwrap(), "FOO");

    let bar = runtime
        .eval(r#"__otter_modules["file:///test/reexport.js"].renamedBar"#)
        .unwrap();
    assert_eq!(bar.to_string().unwrap(), "BAR");

    // baz should not be exported through reexport.js
    let baz = runtime
        .eval(r#"__otter_modules["file:///test/reexport.js"].baz"#)
        .unwrap();
    assert!(baz.is_undefined());
}

/// Test export all (*) functionality
#[test]
fn test_export_all() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    // Base module
    let empty_deps = HashMap::new();
    let base_source = r#"
        export const a = 1;
        export const b = 2;
        export const c = 3;
    "#;

    // Re-export all
    let mut reexport_deps = HashMap::new();
    reexport_deps.insert("./base.js".to_string(), "file:///test/base.js".to_string());
    let reexport_source = r#"
        export * from './base.js';
        export const d = 4;
    "#;

    let modules = vec![
        ("file:///test/base.js", base_source, &empty_deps),
        ("file:///test/all.js", reexport_source, &reexport_deps),
    ];

    let bundle = bundle_modules(modules);
    runtime.eval(&bundle).unwrap();

    // All base exports should be available
    let a = runtime
        .eval(r#"__otter_modules["file:///test/all.js"].a"#)
        .unwrap();
    assert_eq!(a.to_number().unwrap(), 1.0);

    let b = runtime
        .eval(r#"__otter_modules["file:///test/all.js"].b"#)
        .unwrap();
    assert_eq!(b.to_number().unwrap(), 2.0);

    let c = runtime
        .eval(r#"__otter_modules["file:///test/all.js"].c"#)
        .unwrap();
    assert_eq!(c.to_number().unwrap(), 3.0);

    // Plus the module's own export
    let d = runtime
        .eval(r#"__otter_modules["file:///test/all.js"].d"#)
        .unwrap();
    assert_eq!(d.to_number().unwrap(), 4.0);
}

/// Test side-effect only import
#[test]
fn test_side_effect_import() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    // Side-effect module that modifies global
    let empty_deps = HashMap::new();
    let polyfill_source = r#"
        globalThis.__polyfill_loaded = true;
    "#;

    // Main module that imports for side-effect
    let mut main_deps = HashMap::new();
    main_deps.insert(
        "./polyfill.js".to_string(),
        "file:///test/polyfill.js".to_string(),
    );
    let main_source = r#"
        import './polyfill.js';
        export const ready = globalThis.__polyfill_loaded;
    "#;

    let modules = vec![
        ("file:///test/polyfill.js", polyfill_source, &empty_deps),
        ("file:///test/main.js", main_source, &main_deps),
    ];

    let bundle = bundle_modules(modules);
    runtime.eval(&bundle).unwrap();

    let ready = runtime
        .eval(r#"__otter_modules["file:///test/main.js"].ready"#)
        .unwrap();
    assert!(ready.to_bool());
}

/// Test async function export
#[test]
fn test_async_function_export() {
    let runtime = JscRuntime::new(JscConfig::default()).unwrap();

    let deps = HashMap::new();
    let source = r#"
        export async function fetchData() {
            return Promise.resolve({ status: "ok" });
        }
    "#;

    let transformed = transform_module(source, "file:///test/async.js", &deps);
    let wrapped = wrap_module("file:///test/async.js", &transformed);

    let bundle = format!(
        "globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{}",
        wrapped
    );

    runtime.eval(&bundle).unwrap();

    // The async function should be exported
    let is_fn = runtime
        .eval(r#"typeof __otter_modules["file:///test/async.js"].fetchData === 'function'"#)
        .unwrap();
    assert!(is_fn.to_bool());
}
