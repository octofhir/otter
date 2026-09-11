//! Machine IR explicit-receiver calls.
//!
//! # Contents
//! - `receiver.method(this.field)` — the compiler reads the callee before the
//!   property-loading argument and emits `CallWithThis`; a monomorphic site
//!   takes one identity-guarded generated edge.
//! - A plain call whose callee alternates between two functions, which the
//!   profile cannot settle; it completes through the generic explicit-receiver
//!   value call with an `undefined` receiver instead of keeping the function
//!   off the optimizing tier.
//! - A receiver call whose callee identity changes after the edge was built,
//!   so the guard miss completes the call in place.
//!
//! # Invariants
//! - Every tier returns the interpreter's completion.
//! - Each fixture function is compiled by the optimizing tier; explicit
//!   receivers and unsettled plain calls no longer decline the function.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitDebugCompileOutcome, JitDebugEvent, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    SourceInput,
};

const SOURCE: &str = r#"
function Node(v) { this.v = v; this.next = null; }
Node.prototype.sum = function (acc) { return acc + this.v; };
Node.prototype.dup = function (acc) { return acc + this.v * 2; };
function Holder(node) { this.node = node; this.total = 0; }

function add(a, b) { return a + b; }
function mul(a, b) { return a * b; }

function stepMono(h) {
  h.total = h.node.sum(h.total);
  return h.total;
}
function stepPoly(h, i) {
  const fn = (i & 1) === 0 ? add : mul;
  h.total = fn(h.total, 2);
  return h.total;
}
function stepSwitch(h, node) {
  h.total = node.sum(h.total);
  return h.total;
}

const holder = new Holder(new Node(3));
let acc = 0;
for (let i = 0; i < 8000; i++) {
  acc += stepMono(holder);
  if (holder.total > 1000000) holder.total = 0;
}
const poly = new Holder(new Node(1));
for (let i = 0; i < 8000; i++) {
  acc += stepPoly(poly, i);
  if (poly.total > 1000000) poly.total = 0;
}
const swap = new Holder(new Node(5));
const plain = new Node(5);
for (let i = 0; i < 8000; i++) {
  acc += stepSwitch(swap, plain);
  if (swap.total > 1000000) swap.total = 0;
}
// The callee identity behind `node.sum` changes after the edge was built.
Node.prototype.sum = Node.prototype.dup;
for (let i = 0; i < 64; i++) acc += stepSwitch(swap, plain);
acc + "|" + holder.total + "|" + poly.total + "|" + swap.total;
"#;

fn run(selection: JitSelection) -> (String, Vec<String>) {
    // The driving loops stay in the interpreter so every call enters the
    // fixture functions through the interpreter's entry path, which is what
    // accumulates optimizing-tier hotness today.
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-machine-call-with-this.js",
        )
        .expect("explicit-receiver calls");
    let completion = result.completion_string().to_owned();
    let report = result.jit_debug_report().expect("events enabled");
    let mut names = std::collections::BTreeMap::new();
    for event in report.events() {
        if let JitDebugEvent::CompilePrepared {
            function_id,
            function_name,
            ..
        } = event
        {
            names.insert(*function_id, function_name.clone());
        }
    }
    let optimized = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::CompileFinished {
                function_id,
                tier: JitDebugTier::Optimizing,
                outcome: JitDebugCompileOutcome::Compiled { .. },
                ..
            } => names.get(function_id).cloned(),
            _ => None,
        })
        .collect();
    (completion, optimized)
}

#[test]
fn explicit_receiver_calls_match_the_interpreter_on_every_tier() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, _) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
    }
}

#[test]
fn explicit_receiver_and_unsettled_plain_calls_stay_on_the_optimizing_tier() {
    let (_, optimized) = run(JitSelection::ProductionTiered);
    for name in ["stepMono", "stepPoly", "stepSwitch"] {
        assert!(
            optimized.iter().any(|compiled| compiled == name),
            "{name} must compile on the optimizing tier: {optimized:?}"
        );
    }
}
