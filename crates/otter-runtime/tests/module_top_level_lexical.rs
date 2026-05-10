//! Slice A regression coverage for ESM module-init top-level
//! lexical predeclaration and the linker's per-fragment constant
//! pool merge.
//!
//! These tests pin two foundational gaps surfaced by the
//! P2.1 audit (`ENGINE_REFACTOR_EXECUTION_PLAN.md` §P2.1):
//!
//! 1. `hoist_lexical_names`/`hoist_var_names_in_stmt`/
//!    `hoist_function_declarations` must walk through
//!    `ExportNamedDeclaration` and `ExportDefaultDeclaration` so
//!    `export const|let|var|class|function` is registered in the
//!    module-scope binding env before module-init evaluation
//!    starts (§16.2.1.6 `Source Text Module Record
//!    InitializeEnvironment` step 9).
//! 2. The module-graph linker must offset every per-fragment
//!    `Operand::ConstIndex` slot that references the constant
//!    pool, including `Op::LoadGlobalOrThrow`/
//!    `Op::LoadGlobalOrUndefined`. Mis-classifying these operands
//!    as raw counts (or vice versa for `Op::*Call` method-id
//!    slots) silently rebinds free identifiers / call targets to
//!    constants from a different module fragment.
//!
//! See: <https://tc39.es/ecma262/#sec-source-text-module-records>

use otter_runtime::Otter;

/// `import * as ns + console.log(ns.greeting)` exercises the
/// importer-side `LoadGlobalOrThrow` for `console`, which the
/// pre-fix linker wasn't offsetting and so resolved to whichever
/// string lived at the unrewritten constant index in the merged
/// pool.
#[test]
fn cross_module_global_lookup_after_fragment_merge() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("lib.ts"),
        "export const greeting = \"hello\";\n",
    )
    .expect("write lib");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            import * as lib from "./lib.ts";
            if (lib.greeting !== "hello") {
                throw new Error("namespace export wrong: " + lib.greeting);
            }
            // Free identifier `Object` — the linker must offset
            // its constant index past every dependency fragment's
            // pool entries.
            if (typeof Object.keys !== "function") {
                throw new Error("Object.keys lookup corrupted by linker offset");
            }
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .expect("run entry");
}

/// `Math.abs` after fragment merge — the existing `is_const_pool_ref`
/// table was offsetting the `MathCall` method-id slot, silently
/// rebinding to a different `MathMethod` after merge. Cover the
/// fix by computing a known result through `Math.abs` from a
/// module that has dependencies.
#[test]
fn cross_module_method_id_call_after_fragment_merge() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("lib.ts"),
        "export const a = 1;\nexport const b = 2;\nexport const c = 3;\n",
    )
    .expect("write lib");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            import { a, b, c } from "./lib.ts";
            const sum = a + b + c;
            if (Math.abs(-sum) !== 6) {
                throw new Error("Math.abs result corrupted: " + Math.abs(-sum));
            }
            if (Math.max(a, b, c) !== 3) {
                throw new Error("Math.max result corrupted: " + Math.max(a, b, c));
            }
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .expect("run entry");
}

/// `export const|let` must be predeclared in the module env
/// before the body runs so an inner function can capture it as a
/// forward reference. Pre-fix this raised
/// `ReferenceError: greeting is not defined`.
#[test]
fn export_const_forward_reference_from_inner_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            function fetchGreeting() { return greeting; }
            export const greeting = "hello";
            if (fetchGreeting() !== "hello") {
                throw new Error("export const forward ref broken: " + fetchGreeting());
            }
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .expect("run entry");
}

/// `export var x` must be picked up by the var-hoist pass, even
/// when wrapped in an `ExportNamedDeclaration`. Pre-fix this
/// failed compilation with `export var X not pre-hoisted`.
#[test]
fn export_var_hoisted_into_module_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            // Reading the var before its source-position assignment
            // must observe the var-hoist binding initialised to
            // `undefined`, not a ReferenceError.
            const probe = greeting;
            export var greeting = "hello";
            if (probe !== undefined) {
                throw new Error("export var not hoisted; probe=" + probe);
            }
            if (greeting !== "hello") {
                throw new Error("export var assignment lost: " + greeting);
            }
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .expect("run entry");
}

/// `export class C` must be predeclared (TDZ until source
/// position) so an inner function can refer to it via the regular
/// upvalue cascade. Pre-fix this raised
/// `ReferenceError: C is not defined` (the linker bug also
/// surfaced here when the importer module touched globals).
#[test]
fn export_class_forward_reference_from_inner_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            function makeBox() { return new C(7); }
            export class C {
                constructor(x: number) { this.x = x; }
            }
            const box = makeBox();
            if (box.x !== 7) {
                throw new Error("export class forward ref broken: " + box.x);
            }
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .expect("run entry");
}

/// `export function f` must hoist as a HoistableDeclaration so
/// pre-source-position calls observe the bound closure.
#[test]
fn export_function_hoisted_above_source_position() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            const eager = greet("world");
            export function greet(who: string): string { return "hello, " + who; }
            if (eager !== "hello, world") {
                throw new Error("export function not hoisted: " + eager);
            }
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .expect("run entry");
}

/// Mixed `import * as ns + export const` plus a global-identifier
/// reference in the importer. The audit's original two-file
/// repro: pre-fix this failed with
/// `ReferenceError: greeting is not defined` because the
/// `console` constant index landed on `greeting` after the merge.
#[test]
fn mixed_namespace_import_with_global_console() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("lib.ts"),
        "export const greeting = \"hello\";\n",
    )
    .expect("write lib");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            import * as lib from "./lib.ts";
            if (typeof console.log !== "function") {
                throw new Error("console.log lookup broke after fragment merge");
            }
            if (lib.greeting !== "hello") {
                throw new Error("namespace export wrong: " + lib.greeting);
            }
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .expect("run entry");
}
