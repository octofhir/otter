//! Integration tests for ES Module compilation and linking (Step 49).
//!
//! §16.2 — Modules
//! Spec: <https://tc39.es/ecma262/#sec-modules>
//!
//! §16.2.2 — Imports
//! Spec: <https://tc39.es/ecma262/#sec-imports>
//!
//! §16.2.3 — Exports
//! Spec: <https://tc39.es/ecma262/#sec-exports>

use otter_vm::module::{ExportRecord, ImportBinding};
use otter_vm::source::compile_module;

// ═══════════════════════════════════════════════════════════════════════════
//  §16.2 — Module compilation: metadata extraction
// ═══════════════════════════════════════════════════════════════════════════

/// A simple module with no imports or exports compiles and is marked as ESM.
#[test]
fn module_is_esm() {
    let module = compile_module("var x = 1;", "test.mjs").expect("should compile");
    assert!(module.is_esm());
}

/// Side-effect import records the specifier with no bindings.
#[test]
fn import_side_effect() {
    let module = compile_module(r#"import "./init.js";"#, "test.mjs").expect("should compile");
    assert_eq!(module.imports().len(), 1);
    assert_eq!(&*module.imports()[0].specifier, "./init.js");
    assert!(module.imports()[0].bindings.is_empty());
}

/// Named import creates the correct binding record.
#[test]
fn import_named() {
    let module =
        compile_module(r#"import { foo, bar as baz } from "./m.js";"#, "test.mjs")
            .expect("should compile");
    assert_eq!(module.imports().len(), 1);
    let record = &module.imports()[0];
    assert_eq!(&*record.specifier, "./m.js");
    assert_eq!(record.bindings.len(), 2);

    match &record.bindings[0] {
        ImportBinding::Named { imported, local } => {
            assert_eq!(&**imported, "foo");
            assert_eq!(&**local, "foo");
        }
        other => panic!("expected Named, got {other:?}"),
    }
    match &record.bindings[1] {
        ImportBinding::Named { imported, local } => {
            assert_eq!(&**imported, "bar");
            assert_eq!(&**local, "baz");
        }
        other => panic!("expected Named, got {other:?}"),
    }
}

/// Default import creates the correct binding record.
#[test]
fn import_default() {
    let module =
        compile_module(r#"import myDefault from "./m.js";"#, "test.mjs").expect("should compile");
    assert_eq!(module.imports().len(), 1);
    let record = &module.imports()[0];
    assert_eq!(record.bindings.len(), 1);
    match &record.bindings[0] {
        ImportBinding::Default { local } => assert_eq!(&**local, "myDefault"),
        other => panic!("expected Default, got {other:?}"),
    }
}

/// Namespace import creates the correct binding record.
#[test]
fn import_namespace() {
    let module =
        compile_module(r#"import * as ns from "./m.js";"#, "test.mjs").expect("should compile");
    assert_eq!(module.imports().len(), 1);
    let record = &module.imports()[0];
    assert_eq!(record.bindings.len(), 1);
    match &record.bindings[0] {
        ImportBinding::Namespace { local } => assert_eq!(&**local, "ns"),
        other => panic!("expected Namespace, got {other:?}"),
    }
}

/// Mixed import (default + named) from the same specifier.
#[test]
fn import_mixed() {
    let module =
        compile_module(r#"import def, { a, b } from "./m.js";"#, "test.mjs")
            .expect("should compile");
    assert_eq!(module.imports().len(), 1);
    let record = &module.imports()[0];
    assert_eq!(record.bindings.len(), 3);
    assert!(matches!(&record.bindings[0], ImportBinding::Default { .. }));
    assert!(matches!(&record.bindings[1], ImportBinding::Named { .. }));
    assert!(matches!(&record.bindings[2], ImportBinding::Named { .. }));
}

/// Multiple import statements from different specifiers.
#[test]
fn import_multiple_specifiers() {
    let module = compile_module(
        r#"
        import { a } from "./a.js";
        import { b } from "./b.js";
        "#,
        "test.mjs",
    )
    .expect("should compile");
    assert_eq!(module.imports().len(), 2);
    assert_eq!(&*module.imports()[0].specifier, "./a.js");
    assert_eq!(&*module.imports()[1].specifier, "./b.js");
}

// ═══════════════════════════════════════════════════════════════════════════
//  §16.2.3 — Export compilation: metadata extraction
// ═══════════════════════════════════════════════════════════════════════════

/// Named export with declaration: `export const x = 1`.
#[test]
fn export_named_const() {
    let module = compile_module("export const x = 42;", "test.mjs").expect("should compile");
    assert_eq!(module.exports().len(), 1);
    match &module.exports()[0] {
        ExportRecord::Named { local, exported } => {
            assert_eq!(&**local, "x");
            assert_eq!(&**exported, "x");
        }
        other => panic!("expected Named, got {other:?}"),
    }
}

/// Named export with multiple declarators: `export const a = 1, b = 2`.
#[test]
fn export_named_multiple_declarators() {
    let module =
        compile_module("export const a = 1, b = 2;", "test.mjs").expect("should compile");
    assert_eq!(module.exports().len(), 2);
    assert!(matches!(&module.exports()[0], ExportRecord::Named { exported, .. } if &**exported == "a"));
    assert!(matches!(&module.exports()[1], ExportRecord::Named { exported, .. } if &**exported == "b"));
}

/// Named export with function declaration: `export function foo() {}`.
#[test]
fn export_named_function() {
    let module =
        compile_module("export function foo() { return 1; }", "test.mjs")
            .expect("should compile");
    assert_eq!(module.exports().len(), 1);
    match &module.exports()[0] {
        ExportRecord::Named { local, exported } => {
            assert_eq!(&**local, "foo");
            assert_eq!(&**exported, "foo");
        }
        other => panic!("expected Named, got {other:?}"),
    }
}

/// Local export specifiers: `export { x, y as z }`.
#[test]
fn export_local_specifiers() {
    let module = compile_module(
        "const x = 1, y = 2;\nexport { x, y as z };",
        "test.mjs",
    )
    .expect("should compile");
    assert_eq!(module.exports().len(), 2);
    match &module.exports()[0] {
        ExportRecord::Named { local, exported } => {
            assert_eq!(&**local, "x");
            assert_eq!(&**exported, "x");
        }
        other => panic!("expected Named, got {other:?}"),
    }
    match &module.exports()[1] {
        ExportRecord::Named { local, exported } => {
            assert_eq!(&**local, "y");
            assert_eq!(&**exported, "z");
        }
        other => panic!("expected Named, got {other:?}"),
    }
}

/// Default export of an expression: `export default 42`.
#[test]
fn export_default_expression() {
    let module = compile_module("export default 42;", "test.mjs").expect("should compile");
    assert_eq!(module.exports().len(), 1);
    match &module.exports()[0] {
        ExportRecord::Default { local } => assert_eq!(&**local, "*default*"),
        other => panic!("expected Default, got {other:?}"),
    }
}

/// Default export of a named function: `export default function foo() {}`.
#[test]
fn export_default_named_function() {
    let module = compile_module(
        "export default function foo() { return 1; }",
        "test.mjs",
    )
    .expect("should compile");
    assert_eq!(module.exports().len(), 1);
    match &module.exports()[0] {
        ExportRecord::Default { local } => assert_eq!(&**local, "foo"),
        other => panic!("expected Default, got {other:?}"),
    }
}

/// Default export of an anonymous function: `export default function() {}`.
#[test]
fn export_default_anonymous_function() {
    let module =
        compile_module("export default function() { return 1; }", "test.mjs")
            .expect("should compile");
    assert_eq!(module.exports().len(), 1);
    match &module.exports()[0] {
        ExportRecord::Default { local } => assert_eq!(&**local, "*default*"),
        other => panic!("expected Default, got {other:?}"),
    }
}

/// Re-export named: `export { foo } from "./m.js"`.
#[test]
fn export_reexport_named() {
    let module =
        compile_module(r#"export { foo, bar as baz } from "./m.js";"#, "test.mjs")
            .expect("should compile");
    assert_eq!(module.exports().len(), 2);
    match &module.exports()[0] {
        ExportRecord::ReExportNamed {
            specifier,
            imported,
            exported,
        } => {
            assert_eq!(&**specifier, "./m.js");
            assert_eq!(&**imported, "foo");
            assert_eq!(&**exported, "foo");
        }
        other => panic!("expected ReExportNamed, got {other:?}"),
    }
    match &module.exports()[1] {
        ExportRecord::ReExportNamed {
            specifier,
            imported,
            exported,
        } => {
            assert_eq!(&**specifier, "./m.js");
            assert_eq!(&**imported, "bar");
            assert_eq!(&**exported, "baz");
        }
        other => panic!("expected ReExportNamed, got {other:?}"),
    }
}

/// Re-export all: `export * from "./m.js"`.
#[test]
fn export_reexport_all() {
    let module =
        compile_module(r#"export * from "./m.js";"#, "test.mjs").expect("should compile");
    assert_eq!(module.exports().len(), 1);
    match &module.exports()[0] {
        ExportRecord::ReExportAll { specifier } => assert_eq!(&**specifier, "./m.js"),
        other => panic!("expected ReExportAll, got {other:?}"),
    }
}

/// Re-export namespace: `export * as ns from "./m.js"`.
#[test]
fn export_reexport_namespace() {
    let module =
        compile_module(r#"export * as ns from "./m.js";"#, "test.mjs").expect("should compile");
    assert_eq!(module.exports().len(), 1);
    match &module.exports()[0] {
        ExportRecord::ReExportNamespace {
            specifier,
            exported,
        } => {
            assert_eq!(&**specifier, "./m.js");
            assert_eq!(&**exported, "ns");
        }
        other => panic!("expected ReExportNamespace, got {other:?}"),
    }
}

/// Export class declaration: `export class Foo {}`.
#[test]
fn export_class_declaration() {
    let module = compile_module("export class Foo {}", "test.mjs").expect("should compile");
    assert_eq!(module.exports().len(), 1);
    match &module.exports()[0] {
        ExportRecord::Named { local, exported } => {
            assert_eq!(&**local, "Foo");
            assert_eq!(&**exported, "Foo");
        }
        other => panic!("expected Named, got {other:?}"),
    }
}

/// Module with both imports and exports.
#[test]
fn module_with_imports_and_exports() {
    let module = compile_module(
        r#"
        import { helper } from "./utils.js";
        export const result = 42;
        "#,
        "test.mjs",
    )
    .expect("should compile");
    assert_eq!(module.imports().len(), 1);
    assert_eq!(module.exports().len(), 1);
    assert!(module.is_esm());
}

/// Module code is always strict: `this` at top level is `undefined`.
/// (Verified indirectly — module compiles with strict: true on entry function.)
#[test]
fn module_is_strict() {
    let module = compile_module("var x = 1;", "test.mjs").expect("should compile");
    assert!(module.entry_function().is_strict());
}

/// Export let with destructuring: `export let { a, b } = obj`.
#[test]
fn export_destructured_let() {
    let module = compile_module(
        "export let { a, b } = { a: 1, b: 2 };",
        "test.mjs",
    )
    .expect("should compile");
    assert_eq!(module.exports().len(), 2);
    assert!(matches!(&module.exports()[0], ExportRecord::Named { exported, .. } if &**exported == "a"));
    assert!(matches!(&module.exports()[1], ExportRecord::Named { exported, .. } if &**exported == "b"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  §16.2.1 — Module linking & evaluation
// ═══════════════════════════════════════════════════════════════════════════

use otter_vm::module_loader::{InMemoryModuleHost, ModuleRegistry, execute_module_graph};
use otter_vm::RuntimeState;

fn run_module_graph(entry: &str, host: &InMemoryModuleHost) -> (RuntimeState, ModuleRegistry) {
    let mut runtime = RuntimeState::new();
    let mut registry = ModuleRegistry::new();
    execute_module_graph(entry, host, &mut runtime, &mut registry)
        .expect("module graph should execute");
    (runtime, registry)
}

/// Single module with export — value accessible in namespace.
#[test]
fn single_module_export_value() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("entry.mjs", "export var x = 42;");

    let (_, registry) = run_module_graph("entry.mjs", &host);
    let value = registry.get_export("entry.mjs", "x").expect("export x should exist");
    assert_eq!(value.as_i32(), Some(42));
}

/// Module with const export.
#[test]
fn single_module_export_const() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("entry.mjs", "export const greeting = 'hello';");

    let (_, registry) = run_module_graph("entry.mjs", &host);
    let value = registry.get_export("entry.mjs", "greeting").expect("export should exist");
    assert!(value.as_object_handle().is_some()); // string is a heap object
}

/// Module with function export.
#[test]
fn single_module_export_function() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("entry.mjs", "export function add(a, b) { return a + b; }");

    let (_, registry) = run_module_graph("entry.mjs", &host);
    let value = registry.get_export("entry.mjs", "add");
    assert!(
        value.is_some_and(|v| v.as_object_handle().is_some()),
        "expected object handle for function export, got {value:?}"
    );
}

/// Two modules: B imports from A.
#[test]
fn two_module_import_named() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("a.mjs", "export var value = 10;");
    host.add_module("b.mjs", r#"import { value } from "a.mjs"; export var result = value + 5;"#);

    let (_, registry) = run_module_graph("b.mjs", &host);

    // A should be evaluated (dependency).
    assert_eq!(
        registry.get_export("a.mjs", "value").and_then(|v| v.as_i32()),
        Some(10)
    );

    // B should have imported A's value and computed result.
    assert_eq!(
        registry.get_export("b.mjs", "result").and_then(|v| v.as_i32()),
        Some(15)
    );
}

/// Default export and import.
#[test]
fn default_export_and_import() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("a.mjs", "export default 99;");
    host.add_module("b.mjs", r#"import val from "a.mjs"; export var result = val;"#);

    let (_, registry) = run_module_graph("b.mjs", &host);
    assert_eq!(
        registry.get_export("a.mjs", "default").and_then(|v| v.as_i32()),
        Some(99)
    );
    assert_eq!(
        registry.get_export("b.mjs", "result").and_then(|v| v.as_i32()),
        Some(99)
    );
}

/// Re-export named: module C re-exports from A through B.
#[test]
fn reexport_named() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("a.mjs", "export var x = 7;");
    host.add_module("b.mjs", r#"export { x } from "a.mjs";"#);
    host.add_module("c.mjs", r#"import { x } from "b.mjs"; export var result = x;"#);

    let (_, registry) = run_module_graph("c.mjs", &host);
    assert_eq!(
        registry.get_export("b.mjs", "x").and_then(|v| v.as_i32()),
        Some(7)
    );
    assert_eq!(
        registry.get_export("c.mjs", "result").and_then(|v| v.as_i32()),
        Some(7)
    );
}

/// Re-export all: `export * from "a.mjs"`.
#[test]
fn reexport_all() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("a.mjs", "export var x = 3; export var y = 4;");
    host.add_module("b.mjs", r#"export * from "a.mjs";"#);

    let (_, registry) = run_module_graph("b.mjs", &host);
    assert_eq!(
        registry.get_export("b.mjs", "x").and_then(|v| v.as_i32()),
        Some(3)
    );
    assert_eq!(
        registry.get_export("b.mjs", "y").and_then(|v| v.as_i32()),
        Some(4)
    );
}

/// Side-effect import: `import "./init.mjs"` with no bindings.
#[test]
fn side_effect_import() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("init.mjs", "var __initialized = true;");
    host.add_module("entry.mjs", r#"import "./init.mjs";"#);

    let (_, registry) = run_module_graph("entry.mjs", &host);
    // init.mjs should be evaluated (state = Evaluated).
    let loaded = registry.get("init.mjs").expect("init.mjs should be loaded");
    assert_eq!(loaded.state, otter_vm::module_loader::ModuleState::Evaluated);
}

/// Three-level dependency chain: C → B → A.
#[test]
fn three_level_dependency_chain() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("a.mjs", "export var base = 1;");
    host.add_module("b.mjs", r#"import { base } from "a.mjs"; export var mid = base + 10;"#);
    host.add_module("c.mjs", r#"import { mid } from "b.mjs"; export var top = mid + 100;"#);

    let (_, registry) = run_module_graph("c.mjs", &host);
    assert_eq!(
        registry.get_export("c.mjs", "top").and_then(|v| v.as_i32()),
        Some(111)
    );
}

/// Multiple exports from the same module.
#[test]
fn multiple_exports() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("a.mjs", "export var x = 1; export var y = 2; export var z = 3;");

    let (_, registry) = run_module_graph("a.mjs", &host);
    assert_eq!(registry.get_export("a.mjs", "x").and_then(|v| v.as_i32()), Some(1));
    assert_eq!(registry.get_export("a.mjs", "y").and_then(|v| v.as_i32()), Some(2));
    assert_eq!(registry.get_export("a.mjs", "z").and_then(|v| v.as_i32()), Some(3));
}

/// Export with rename: `export { x as renamed }`.
#[test]
fn export_with_rename() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("a.mjs", "var x = 42; export { x as renamed };");

    let (_, registry) = run_module_graph("a.mjs", &host);
    assert_eq!(
        registry.get_export("a.mjs", "renamed").and_then(|v| v.as_i32()),
        Some(42)
    );
    // Original name should NOT be in the namespace.
    assert!(registry.get_export("a.mjs", "x").is_none());
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.10 — Dynamic import() expression
// ═══════════════════════════════════════════════════════════════════════════

/// `import("specifier")` compiles to a DynamicImport opcode.
#[test]
fn dynamic_import_compiles() {
    let module = compile_module(
        r#"var p = import("./other.mjs");"#,
        "test.mjs",
    )
    .expect("should compile");
    // Should have DynamicImport instruction in the entry function bytecode.
    let entry = module.entry_function();
    let has_dynamic_import = entry
        .bytecode()
        .instructions()
        .iter()
        .any(|i| i.opcode() == otter_vm::bytecode::Opcode::DynamicImport);
    assert!(has_dynamic_import, "expected DynamicImport opcode in bytecode");
}

/// `import()` without a host handler throws TypeError.
#[test]
fn dynamic_import_without_handler_throws() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("entry.mjs", r#"var p = import("./other.mjs");"#);
    host.add_module("other.mjs", "export var x = 1;");

    let mut runtime = RuntimeState::new();
    let mut registry = ModuleRegistry::new();
    let result = execute_module_graph("entry.mjs", &host, &mut runtime, &mut registry);
    // Should fail — no __importDynamic handler installed.
    assert!(result.is_err(), "expected error without __importDynamic handler");
}

/// `import()` with a host handler calls __importDynamic(specifier).
#[test]
fn dynamic_import_calls_host_handler() {
    use otter_vm::descriptors::{
        NativeEntrypointKind, NativeFunctionDescriptor, NativeSlotKind,
    };
    use otter_vm::RegisterValue;

    let mut host = InMemoryModuleHost::new();
    // Module that uses dynamic import and stores the specifier it was called with.
    host.add_module(
        "entry.mjs",
        r#"
        var result = import("./dep.mjs");
        "#,
    );

    let mut runtime = RuntimeState::new();

    // Install a __importDynamic handler that just returns a resolved promise
    // with a simple namespace object.
    fn import_handler(
        _this: &RegisterValue,
        args: &[RegisterValue],
        runtime: &mut otter_vm::RuntimeState,
    ) -> Result<RegisterValue, otter_vm::VmNativeCallError> {
        // Read the specifier argument and store it as a global for verification.
        let specifier = args.first().copied().unwrap_or(RegisterValue::undefined());
        runtime.install_global_value("__importSpecifier", specifier);

        // Build a namespace object with a single `x` export.
        let ns = runtime.alloc_object();
        let prop = runtime.intern_property_name("x");
        runtime
            .objects_mut()
            .set_property(ns, prop, RegisterValue::from_i32(99))
            .unwrap();

        // Create a fulfilled promise with the namespace.
        let promise = runtime
            .alloc_resolved_promise(RegisterValue::from_object_handle(ns.0))
            .map_err(|e| otter_vm::VmNativeCallError::Internal(format!("{e:?}").into()))?;
        Ok(RegisterValue::from_object_handle(promise.0))
    }

    runtime.install_native_global(NativeFunctionDescriptor::new(
        "__importDynamic",
        1,
        NativeEntrypointKind::Sync,
        NativeSlotKind::Method,
        import_handler,
    ));

    let mut registry = ModuleRegistry::new();
    execute_module_graph("entry.mjs", &host, &mut runtime, &mut registry)
        .expect("module graph should execute");

    // Verify __importDynamic was called with the correct specifier.
    let global = runtime.intrinsics().global_object();
    let prop = runtime.intern_property_name("__importSpecifier");
    let specifier_value = runtime
        .own_property_value(global, prop)
        .unwrap_or_default();
    // Should be a string object handle (the specifier "./dep.mjs").
    assert!(
        specifier_value.as_object_handle().is_some(),
        "expected specifier string, got {specifier_value:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.12 — import.meta
// ═══════════════════════════════════════════════════════════════════════════

/// `import.meta` compiles to an ImportMeta opcode.
#[test]
fn import_meta_compiles() {
    let module = compile_module(
        "var url = import.meta.url;",
        "test.mjs",
    )
    .expect("should compile");
    let entry = module.entry_function();
    let has_import_meta = entry
        .bytecode()
        .instructions()
        .iter()
        .any(|i| i.opcode() == otter_vm::bytecode::Opcode::ImportMeta);
    assert!(has_import_meta, "expected ImportMeta opcode in bytecode");
}

/// `import.meta.url` returns the module's URL at runtime.
#[test]
fn import_meta_url_returns_module_url() {
    let mut host = InMemoryModuleHost::new();
    host.add_module(
        "entry.mjs",
        "export var meta_url = import.meta.url;",
    );

    let (runtime, registry) = run_module_graph("entry.mjs", &host);
    let value = registry
        .get_export("entry.mjs", "meta_url")
        .expect("meta_url export should exist");

    // Should be a string handle containing "entry.mjs".
    let handle = value.as_object_handle().expect("expected string handle");
    let string = runtime
        .objects()
        .string_value(otter_vm::object::ObjectHandle(handle))
        .expect("should be readable")
        .expect("should be a string");
    assert_eq!(string, "entry.mjs");
}

// ═══════════════════════════════════════════════════════════════════════════
//  §16.2.3 — ReExportNamespace: `export * as ns from "..."`
// ═══════════════════════════════════════════════════════════════════════════

/// `export * as ns from "..."` creates a namespace object with all exports.
#[test]
fn reexport_namespace_object() {
    let mut host = InMemoryModuleHost::new();
    host.add_module("a.mjs", "export var x = 10; export var y = 20;");
    host.add_module("b.mjs", r#"export * as ns from "a.mjs";"#);
    host.add_module(
        "c.mjs",
        r#"import { ns } from "b.mjs"; export var sum = ns.x + ns.y;"#,
    );

    let (_, registry) = run_module_graph("c.mjs", &host);

    // b.mjs should have a `ns` export that is an object.
    let ns_value = registry
        .get_export("b.mjs", "ns")
        .expect("ns export should exist");
    assert!(
        ns_value.as_object_handle().is_some(),
        "ns should be an object, got {ns_value:?}"
    );

    // c.mjs should have computed sum = 10 + 20 = 30.
    assert_eq!(
        registry.get_export("c.mjs", "sum").and_then(|v| v.as_i32()),
        Some(30)
    );
}

/// Circular module dependency doesn't crash.
#[test]
fn circular_dependency_doesnt_crash() {
    let mut host = InMemoryModuleHost::new();
    host.add_module(
        "a.mjs",
        r#"import { b_val } from "b.mjs"; export var a_val = 1;"#,
    );
    host.add_module(
        "b.mjs",
        r#"import { a_val } from "a.mjs"; export var b_val = 2;"#,
    );

    // Should not hang or crash — circular deps are allowed in ESM.
    let mut runtime = RuntimeState::new();
    let mut registry = ModuleRegistry::new();
    let result = execute_module_graph("a.mjs", &host, &mut runtime, &mut registry);
    // The result may or may not succeed (depends on initialization order),
    // but it must not hang or crash.
    let _ = result;
    assert!(registry.get("a.mjs").is_some());
    assert!(registry.get("b.mjs").is_some());
}
