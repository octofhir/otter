//! Production template-tier spread/call-family coverage.
//!
//! # Contents
//! - `CallSpread`, `CallWithThis`, `CollectArguments`, `NewSpread`, and
//!   `SuperConstructSpread` in hot code. `TailCall` is also present in the
//!   fixture but completes through the interpreter (it is excluded from the
//!   compiled set to preserve true tail-call stack reuse); the surrounding
//!   loop still tiers up and reaches it through the reentrant path.
//! - An observable receiver getter proving explicit-`this` evaluation count.
//! - Interpreter/template completion parity.
//!
//! # Invariants
//! - Every compiled call/construct completes through the VM's synchronous
//!   helper and is never replayed after a committed side effect.
//! - Template output is byte-identical to the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_spread_call_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
let getterEffects = 0;
const receiver = {
  get base() {
    getterEffects++;
    return 10;
  }
};

function explicitThis(a, b) { return this.base + a + b; }
function spreadCall(a, b) { return a * 2 + b; }
function argumentsUser(a, b) {
  return arguments[0] + arguments[1] + arguments.length;
}
class Point {
  constructor(a, b) { this.total = a + b; }
}
class Base {
  constructor(a, b) { this.total = a * 3 + b; }
}
class Derived extends Base {
  constructor(...args) { super(...args); }
}
function leaf(a, b) { return a - b; }
function tail(a) { return leaf(a, 4); }

function matrix(rounds) {
  let sum = 0;
  for (let i = 0; i < rounds; i++) {
    sum += explicitThis.call(receiver, i, 1);
    sum += spreadCall(...[i, 2]);
    sum += argumentsUser(i, 3);
    sum += new Point(...[i, 4]).total;
    sum += new Derived(i, 5).total;
    sum += tail(i);
  }
  return JSON.stringify([sum, getterEffects]);
}

matrix(180);
"#;

fn run(selection: JitSelection) -> (String, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(8)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-spread-calls.js",
        )
        .expect("spread/call matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_osr_attempts,
        stats.jit_reentrant_stub_transitions,
    )
}

#[test]
fn spread_call_family_matches_oracle_with_single_getter_evaluation() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert_eq!(compiled, "[149130,180]");
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "spread/call opcodes must use the shared reentrant transition"
    );
}

// Strict-mode proper tail calls run in O(1) call depth. The interpreter's
// `TailCall` completion discards the caller frame for real tail-call reuse; a
// compiled completion that nested the callee instead would exceed the call-depth
// limit and throw `RangeError` where the interpreter returns. `TailCall` is
// therefore excluded from the compiled set (stays an exact side exit), so a hot
// tail-recursive function keeps interpreting and both tiers return identically.
const DEEP_TAIL: &str = r#"
"use strict";
function count(n, acc) {
  if (n === 0) return acc;
  return count(n - 1, acc + 1);
}
String(count(200000, 0));
"#;

fn run_deep_tail(selection: JitSelection) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(8)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(DEEP_TAIL.to_string()),
            "jit-deep-tail.js",
        )
        .expect("deep tail recursion must not overflow the native stack")
        .completion_string()
        .to_owned()
}

#[test]
fn deep_tail_recursion_preserves_stack_reuse_under_template() {
    let oracle = run_deep_tail(JitSelection::InterpreterOnly);
    let compiled = run_deep_tail(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert_eq!(compiled, "200000");
}
