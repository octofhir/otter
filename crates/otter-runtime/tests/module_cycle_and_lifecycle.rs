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
//! 2. The not-yet-evaluated side of the cycle must read as
//!    `undefined` from the partially-populated `module_env`,
//!    rather than crashing or rebinding to a stale value.
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
/// `a.ts` runs `b.ts`'s body first; b's read of `fromA` must
/// observe `undefined` (a hasn't run yet) without throwing. Then
/// `a.ts` runs and reads `fromB` which is now populated.
#[test]
fn two_file_cycle_reads_partial_env_without_throwing() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_pair(
        dir.path(),
        // a.ts
        r#"
            import { fromB } from "./b.ts";
            export const fromA = "A";
            // b.ts evaluates first per post-order DFS rooted at a;
            // by the time a's body runs, fromB has been mirrored
            // through to b's module_env.
            if (fromB !== "B") {
                throw new Error("a.ts saw fromB=" + fromB);
            }
        "#,
        // b.ts
        r#"
            import { fromA } from "./a.ts";
            export const fromB = "B";
            // The cyclic edge is short-circuited at link time —
            // a's <module-init> hasn't run yet so a's module_env
            // is still empty for `fromA`. Spec live-binding
            // semantics surface that as `undefined` rather than
            // a TDZ ReferenceError.
            if (fromA !== undefined) {
                throw new Error("b.ts saw fromA=" + fromA);
            }
        "#,
    );

    Otter::new()
        .blocking_run_file(dir.path().join("a.ts"))
        .expect("run a.ts");
}

/// Symmetric run: entry = b.ts. The post-order flips so a.ts
/// evaluates first; a sees `fromB === undefined`, then b sees
/// `fromA === "A"`. Verifies the cycle handler is symmetric and
/// not entry-biased.
#[test]
fn two_file_cycle_symmetric_when_entry_flips() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_pair(
        dir.path(),
        // a.ts
        r#"
            import { fromB } from "./b.ts";
            export const fromA = "A";
            if (fromB !== undefined) {
                throw new Error("a.ts saw fromB=" + fromB);
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
            // a hasn't run when b's body executes, so fromA reads
            // as undefined here. greet() is a live binding —
            // calling it after both modules finish observes the
            // populated values.
            if (fromA !== undefined) {
                throw new Error("b.ts saw fromA=" + fromA);
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
    std::fs::write(
        dir.path().join("b.ts"),
        r#"
            import { fromC } from "./c.ts";
            import { fromA } from "./a.ts";
            export const fromB = "B";
            if (fromC !== "C") throw new Error("b saw fromC=" + fromC);
            if (fromA !== undefined) throw new Error("b saw fromA=" + fromA);
        "#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("c.ts"),
        r#"
            import { fromA } from "./a.ts";
            export const fromC = "C";
            if (fromA !== undefined) throw new Error("c saw fromA=" + fromA);
        "#,
    )
    .unwrap();

    Otter::new()
        .blocking_run_file(dir.path().join("a.ts"))
        .expect("run a.ts");
}
