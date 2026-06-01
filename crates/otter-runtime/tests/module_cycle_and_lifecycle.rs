//! Slice B regression coverage: ESM cycle support and module
//! record lifecycle transitions.
//!
//! ECMA-262 §16.2.1 Cyclic Module Records require the loader to
//! short-circuit a cyclic edge (the dependency that comes back
//! to a still-in-progress module record) and let live-binding
//! indirection handle late-bound exports at run time. This test
//! pins both halves of that contract:
//!
//! 1. The graph driver must accept a two-file cycle without
//!    raising `MODULE_GRAPH_CYCLE`.
//! 2. The not-yet-evaluated side of the cycle holds its `const` /
//!    `let` exports as uninitialized bindings, so reading one across
//!    the cyclic edge is a Temporal Dead Zone `ReferenceError`
//!    (§16.2.1.7 InitializeEnvironment + GetBindingValue), not a
//!    silent `undefined`. Function and `var` exports stay observable
//!    through live-binding indirection.
//!
//! See: <https://tc39.es/ecma262/#sec-cyclic-module-records>
//!      <https://tc39.es/ecma262/#sec-InnerModuleEvaluation>

use std::path::Path;

use otter_runtime::Otter;

fn write_pair(dir: &Path, a_src: &str, b_src: &str) {
    std::fs::write(dir.join("a.ts"), a_src).expect("write a.ts");
    std::fs::write(dir.join("b.ts"), b_src).expect("write b.ts");
}

/// Two-file ESM cycle, entry = a.ts. Post-order DFS rooted at
/// `a.ts` runs `b.ts`'s body first; b's read of the uninitialized
/// `const fromA` is a TDZ `ReferenceError` (a hasn't run yet). Then
/// `a.ts` runs and reads `fromB`, which is now initialized.
#[test]
fn two_file_cycle_reads_uninitialized_export_as_tdz() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_pair(
        dir.path(),
        // a.ts
        r#"
            import { fromB } from "./b.ts";
            export const fromA = "A";
            // b.ts evaluates first per post-order DFS rooted at a;
            // by the time a's body runs, fromB is initialized.
            if (fromB !== "B") {
                throw new Error("a.ts saw fromB=" + fromB);
            }
        "#,
        // b.ts
        r#"
            import { fromA } from "./a.ts";
            export const fromB = "B";
            // The cyclic edge is short-circuited at link time —
            // a's <module-init> hasn't run yet, so a's `fromA`
            // binding is still uninitialized and reading it is a
            // TDZ ReferenceError.
            let observed;
            try { fromA; observed = "no-throw"; }
            catch (e) { observed = e.constructor.name; }
            if (observed !== "ReferenceError") {
                throw new Error("b.ts saw fromA=" + observed);
            }
        "#,
    );

    Otter::new()
        .blocking_run_file(dir.path().join("a.ts"))
        .expect("run a.ts");
}

/// Symmetric run: entry = b.ts. The post-order flips so a.ts
/// evaluates first; a's read of the uninitialized `const fromB` is
/// a TDZ `ReferenceError`, then b sees `fromA === "A"`. Verifies the
/// cycle handler is symmetric and not entry-biased.
#[test]
fn two_file_cycle_symmetric_when_entry_flips() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_pair(
        dir.path(),
        // a.ts
        r#"
            import { fromB } from "./b.ts";
            export const fromA = "A";
            let observed;
            try { fromB; observed = "no-throw"; }
            catch (e) { observed = e.constructor.name; }
            if (observed !== "ReferenceError") {
                throw new Error("a.ts saw fromB=" + observed);
            }
        "#,
        // b.ts
        r#"
            import { fromA } from "./a.ts";
            export const fromB = "B";
            if (fromA !== "A") {
                throw new Error("b.ts saw fromA=" + fromA);
            }
        "#,
    );

    Otter::new()
        .blocking_run_file(dir.path().join("b.ts"))
        .expect("run b.ts");
}

/// A cycle through a function call: a's exported function reads
/// b's export at call time, after both module bodies have
/// finished. This exercises live-binding indirection past the
/// initial evaluation gap — by the time `a.greet()` runs, b's
/// `fromB` is set.
#[test]
fn cycle_with_late_function_call_observes_full_bindings() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_pair(
        dir.path(),
        // a.ts
        r#"
            import { fromB } from "./b.ts";
            export function greet() { return "a:" + fromB; }
            export const fromA = "A";
        "#,
        // b.ts
        r#"
            import { greet, fromA } from "./a.ts";
            export const fromB = "B";
            // a hasn't run when b's body executes, so `fromA` is an
            // uninitialized binding and reading it is a TDZ
            // ReferenceError. `greet` is a hoisted function binding —
            // calling it after both modules finish observes the
            // populated values through live-binding indirection.
            let observed;
            try { fromA; observed = "no-throw"; }
            catch (e) { observed = e.constructor.name; }
            if (observed !== "ReferenceError") {
                throw new Error("b.ts saw fromA=" + observed);
            }
        "#,
    );

    let entry = dir.path().join("a.ts");
    std::fs::write(
        &entry,
        r#"
            import { fromB } from "./b.ts";
            export function greet() { return "a:" + fromB; }
            export const fromA = "A";
            if (greet() !== "a:B") {
                throw new Error("greet() observed fromB=" + greet());
            }
        "#,
    )
    .expect("rewrite a.ts");

    Otter::new().blocking_run_file(&entry).expect("run a.ts");
}

/// Three-module cycle a → b → c → a verifies the cycle handler
/// scales past the simple two-vertex case. Post-order from `a`
/// is [c, b, a]; c sees b and a as undefined, b sees a as
/// undefined and c populated, a sees b and c populated.
#[test]
fn three_module_cycle_post_order_evaluation() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("a.ts"),
        r#"
            import { fromB } from "./b.ts";
            export const fromA = "A";
            if (fromB !== "B") throw new Error("a saw fromB=" + fromB);
        "#,
    )
    .unwrap();
    // §16.2.1.7 — `fromA` is an imported `const` whose binding is
    // created uninitialized during instantiation. Because the cycle
    // evaluates c, then b, then a (post-order rooted at the entry a),
    // both b and c run before a's body initializes `fromA`, so reading
    // it is a Temporal Dead Zone `ReferenceError`, not `undefined`.
    std::fs::write(
        dir.path().join("b.ts"),
        r#"
            import { fromC } from "./c.ts";
            import { fromA } from "./a.ts";
            export const fromB = "B";
            if (fromC !== "C") throw new Error("b saw fromC=" + fromC);
            let observed;
            try { fromA; observed = "no-throw"; }
            catch (e) { observed = e.constructor.name; }
            if (observed !== "ReferenceError") throw new Error("b saw fromA=" + observed);
        "#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("c.ts"),
        r#"
            import { fromA } from "./a.ts";
            export const fromC = "C";
            let observed;
            try { fromA; observed = "no-throw"; }
            catch (e) { observed = e.constructor.name; }
            if (observed !== "ReferenceError") throw new Error("c saw fromA=" + observed);
        "#,
    )
    .unwrap();

    Otter::new()
        .blocking_run_file(dir.path().join("a.ts"))
        .expect("run a.ts");
}
