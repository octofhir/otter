//! Direct-eval shadowing regressions shared by interpreter and compiled tiers.
//!
//! # Contents
//! - Captured read, write, const-write, and delete through an eval environment.
//! - Descendant-closure visibility of an inherited eval environment.
//! - Captured TDZ fallback when no eval binding exists.
//! - Assignment cases where the right-hand side introduces the eval binding.
//! - Bounded eval-chain lookup that cannot cross the captured declaration.
//! - Sloppy named-function self bindings shadowed by direct eval.
//!
//! # Invariants
//! - Dynamic and shadowed binding operations commit exactly once; compiled
//!   execution never deoptimizes and replays an accessor or eval-chain effect.
//! - A missing eval binding falls back to the captured cell with the same TDZ
//!   and immutable-binding checks as an ordinary captured access.
//! - Interpreter, baseline, and production tiering observe the same binding
//!   semantics.

use otter_runtime::{JitSelection, Runtime, RuntimeExecutionStats, SourceInput};

fn run(source: &str, selection: JitSelection) -> (String, RuntimeExecutionStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("binding-shadow runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(source.to_owned()),
            "jit-binding-eval-shadow.js",
        )
        .expect("binding-shadow fixture")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats())
}

fn assert_all_tiers(source: &str, expected: &str) {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let (actual, stats) = run(source, selection);
        assert_eq!(actual, expected, "selection={selection:?}");
        match selection {
            JitSelection::InterpreterOnly => {}
            JitSelection::Template | JitSelection::ProductionTiered => assert!(
                stats.jit_code_generations > 0 && stats.jit_runtime_stub_transitions > 0,
                "fixture must compile and execute native code: {stats:?}"
            ),
        }
        if !matches!(selection, JitSelection::InterpreterOnly) {
            assert!(
                stats.jit_reentrant_stub_transitions > 0,
                "shadowed binding must cross the committed boundary: {stats:?}"
            );
        }
    }
}

#[test]
fn eval_shadow_owns_captured_write_and_delete() {
    assert_all_tiers(
        r#"
function shadowWriteScenario() {
  let outer = 3;
  function hot() {
    eval("var outer = 11");
    outer = 12;
    const beforeDelete = outer;
    const deleted = delete outer;
    return [beforeDelete, deleted, outer];
  }
  return [hot(), outer];
}
for (let warm = 0; warm < 96; warm++) shadowWriteScenario();
JSON.stringify(shadowWriteScenario());
"#,
        "[[12,true,3],3]",
    );
}

#[test]
fn eval_shadow_bypasses_captured_const_without_mutating_it() {
    assert_all_tiers(
        r#"
function constShadowScenario() {
  const outer = 3;
  function hot() {
    eval("var outer = 11");
    outer = 12;
    return outer;
  }
  return [hot(), outer];
}
for (let warm = 0; warm < 96; warm++) constShadowScenario();
JSON.stringify(constShadowScenario());
"#,
        "[12,3]",
    );
}

#[test]
fn descendant_closure_observes_inherited_eval_chain() {
    assert_all_tiers(
        r#"
function outer() {
  let captured = 3;
  function install() {
    eval("var captured = 11");
    return function descendant() { return captured; };
  }
  const descendant = install();
  return [descendant(), captured];
}
for (let warm = 0; warm < 96; warm++) outer();
JSON.stringify(outer());
"#,
        "[11,3]",
    );
}

#[test]
fn shadowed_fallback_preserves_captured_tdz() {
    assert_all_tiers(
        r#"
function outer() {
  function hot() {
    eval("");
    return captured;
  }
  try { return hot(); } catch (error) { return error.name; }
  let captured = 1;
}
for (let warm = 0; warm < 96; warm++) outer();
outer();
"#,
        "ReferenceError",
    );
}

#[test]
fn rhs_eval_binding_is_used_by_assignment_family() {
    assert_all_tiers(
        r#"
function plain() {
  let captured = 3;
  function hot() {
    captured = eval("var captured; 5");
    return captured;
  }
  return [hot(), captured];
}
function compound() {
  let captured = 3;
  function hot() {
    captured += eval("var captured; 1");
    return captured;
  }
  return [hot(), captured];
}
function logical() {
  let captured = 0;
  function hot() {
    captured ||= eval("var captured; 5");
    return captured;
  }
  return [hot(), captured];
}
for (let warm = 0; warm < 96; warm++) {
  plain();
  compound();
  logical();
}
JSON.stringify([plain(), compound(), logical()]);
"#,
        "[[5,3],[4,3],[5,0]]",
    );
}

#[test]
fn eval_chain_is_bounded_inside_the_capture_owner() {
    assert_all_tiers(
        r#"
function outer() {
  eval("var x = 1");
  function mid() {
    let x = 2;
    function inner() {
      eval("var y = 0");
      const before = x;
      x = 3;
      const removed = delete x;
      return [before, x, removed, y];
    }
    return [inner(), x];
  }
  return [mid(), x];
}
for (let warm = 0; warm < 96; warm++) outer();
JSON.stringify(outer());
"#,
        "[[[2,3,false,0],3],1]",
    );
}

#[test]
fn sloppy_eval_shadows_named_function_self_binding() {
    assert_all_tiers(
        r#"
function namedSelfScenario() {
  return (function f() {
    eval("var f = 3");
    const read = f;
    const post = f++;
    f = 5;
    const removed = delete f;
    const fallback = typeof f;
    f = 6;
    return [read, post, removed, fallback, typeof f];
  })();
}
for (let warm = 0; warm < 96; warm++) namedSelfScenario();
JSON.stringify(namedSelfScenario());
"#,
        "[3,3,true,\"function\",\"function\"]",
    );
}
