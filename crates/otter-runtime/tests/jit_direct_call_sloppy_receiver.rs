//! Generated direct calls into sloppy callees with an explicit receiver.
//!
//! # Contents
//! - A sloppy prototype method called on a local object receiver, the
//!   `LoadProperty` + `CallWithThis` shape every ES5-style program hits, run
//!   hot enough for the caller's direct-call edge to be generated.
//! - The same callee reached with `null`, `undefined`, and a primitive
//!   receiver, so every OrdinaryCallBindThis outcome is compared with the
//!   interpreter.
//!
//! # Invariants
//! - Every tier returns the interpreter's completion.
//! - An Object receiver enters the generated callee without a side exit: the
//!   generated call count dwarfs its deopt count.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function Node(id) { this.id = id; this.link = null; }
Node.prototype.addTo = function (queue) {
  this.link = null;
  if (queue == null) return this;
  var peek, next = queue;
  while ((peek = next.link) != null) next = peek;
  next.link = this;
  return queue;
};
function whoAmI(_) {
  if (this === globalThis) return "global";
  return typeof this === "object" ? Object.prototype.toString.call(this) : typeof this;
}
Object.prototype.who = whoAmI;
String.prototype.who = whoAmI;

// An argument that reads a property makes the compiler read the callee
// first and call through `CallWithThis` with the receiver register, the
// shape every ES5-style `receiver.method(this.field)` takes. The callers are
// entry-compiled functions invoked once per round, so a side exit inside
// them would repeat on every entry instead of disabling one loop header.
function step(state, i) {
  const node = new Node(i);
  state.queue = node.addTo(state.queue);
  if ((i & 15) === 15) { state.acc += state.queue.id; state.queue = null; }
}
function build(rounds) {
  const state = { queue: null, acc: 0 };
  for (let i = 0; i < rounds; i++) step(state, i);
  return state.acc;
}
function probe(holder, text) {
  const object = holder.who(holder.tag);
  const primitive = text.who(holder.tag);
  const viaNull = whoAmI.call(null, holder.tag);
  const viaUndefined = whoAmI.call(undefined, holder.tag);
  return [object, primitive, viaNull, viaUndefined].join("|");
}
function kinds(rounds) {
  let tally = "";
  const holder = { who: whoAmI, tag: 1 };
  for (let i = 0; i < rounds; i++) tally = probe(holder, "s");
  return tally;
}
build(4000) + ";" + kinds(400);
"#;

fn run(selection: JitSelection) -> (String, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-direct-call-sloppy-receiver.js",
        )
        .expect("sloppy receiver calls")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_generated_calls,
        stats.jit_generated_call_deopts,
    )
}

#[test]
fn sloppy_callees_bind_every_receiver_kind_like_the_interpreter() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    assert!(
        oracle.ends_with(";[object Object]|[object String]|global|global"),
        "{oracle}"
    );
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, _, _) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
    }
}

#[test]
fn object_receivers_enter_generated_sloppy_callees_without_side_exits() {
    let (_, generated_calls, generated_deopts) = run(JitSelection::Template);
    assert!(
        generated_calls >= 1000,
        "the hot object-receiver call must take the generated direct-call edge: {generated_calls}"
    );
    // Only the primitive-receiver probe may leave generated code, once per
    // round; an Object receiver never does.
    assert!(
        generated_deopts * 8 < generated_calls,
        "object receivers must not side-exit the generated call: {generated_deopts} deopts of {generated_calls} calls"
    );
}
