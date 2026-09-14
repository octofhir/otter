//! Generated add-transition stores past the inline slot capacity.
//!
//! # Contents
//! - A constructor that appends twelve string-keyed properties, so most of
//!   its stores land in the spilled slot slab rather than the in-body inline
//!   array, run hot enough to compile and compared against the interpreter.
//! - A receiver whose slab was reserved ahead of its length, so an inline
//!   slot index meets a non-null slab handle: a compiled caller constructs
//!   through the generated direct-construct boundary, whose receiver
//!   preparation reserves the whole transition program's capacity before the
//!   first store, and the constructor's non-scalar field values keep it off
//!   the pre-shaped simple-constructor path.
//! - The runtime property-stub count, which must stay bounded by the slab
//!   growth points rather than scale with every store.
//!
//! # Invariants
//! - Every tier produces the interpreter's result for every field.
//! - A compiled add-transition into a slab word with capacity commits in
//!   generated code; only the growth points reach the runtime stub.
//! - Appending slot zero to a receiver that already owns an out-of-line slab
//!   leaves its value base on that slab; every later slab-relative store must
//!   land in the slab, never in the body.
//! - A constructor body proves its receiver's storage capacity before every
//!   baked field transition; a receiver that arrived without the program's
//!   reservation (a foreign `new.target`) exits to the canonical store instead
//!   of writing past the inline words.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function Wide(i) {
  this.f0 = i; this.f1 = i + 1; this.f2 = null; this.f3 = i * 2; this.f4 = -1;
  this.f5 = true; this.f6 = 0; this.f7 = "s" + (i & 3); this.f8 = i; this.f9 = 1;
  this.f10 = { n: i }; this.f11 = false;
}
function widen(rounds) {
  let acc = 0;
  let last = null;
  for (let i = 0; i < rounds; i++) {
    const w = new Wide(i);
    acc += w.f0 + w.f1 + w.f3 + w.f8 + w.f9 + w.f10.n + (w.f5 ? 1 : 0) + w.f7.length;
    last = w;
  }
  const keys = Object.keys(last).join(",");
  return acc + "|" + keys + "|" + last.f2 + "|" + last.f4 + "|" + last.f11;
}
widen(600);
"#;

fn run(selection: JitSelection) -> (String, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-property-transitions.js",
        )
        .expect("wide constructor loop")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_osr_attempts,
        stats.jit_runtime_property_stubs,
    )
}

#[test]
fn slab_add_transitions_match_the_interpreter_on_every_tier() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    assert!(oracle.ends_with("|null|-1|false"), "{oracle}");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, osr_attempts, _) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
        assert!(
            osr_attempts > 0,
            "{selection:?} must enter at a loop OSR header"
        );
    }
}

#[test]
fn slab_add_transitions_stay_in_generated_code() {
    let (_, _, runtime_stubs) = run(JitSelection::Template);
    // Twelve stores per object over six hundred objects: a runtime call per
    // store would be seven thousand. The slab grows twice per object
    // (inline cap 3 → 6 → 12), and the compiled loop only starts after the
    // OSR threshold, so the stub count is a small multiple of the object
    // count, never of the store count.
    assert!(
        runtime_stubs < 600 * 4,
        "runtime property stubs must not scale with every slab store: {runtime_stubs}"
    );
}

const RESERVED_RECEIVER_SOURCE: &str = r#"
function Cell(v) {
  this.a = v; this.b = v + 1; this.c = null; this.d = v * 2; this.e = "x" + (v & 1); this.f = v;
}
function make(v) { return new Cell(v); }
function run(rounds) {
  let acc = 0;
  let keys = "";
  let bad = 0;
  for (let i = 0; i < rounds; i++) {
    const c = make(i);
    acc += c.a + c.b + c.d + c.f + c.e.length;
    if (c.c !== null || c.a !== i || c.f !== i) bad++;
    keys = Object.keys(c).join(",");
  }
  return acc + "|" + keys + "|" + bad;
}
run(400);
"#;

fn run_reserved_receiver(selection: JitSelection) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(RESERVED_RECEIVER_SOURCE.to_string()),
            "jit-reserved-receiver-transitions.js",
        )
        .expect("compiled caller constructing a spilled receiver")
        .completion_string()
        .to_owned()
}

#[test]
fn slot_zero_transition_keeps_a_reserved_slab_as_the_value_base() {
    // The constructor tiers up before its caller does, so the caller's
    // generated construct boundary prepares the receiver: a non-simple
    // constructor body means the receiver arrives with the root shape and a
    // reserved out-of-line slab, and every field is appended by generated
    // add-transitions starting at slot zero.
    let oracle = run_reserved_receiver(JitSelection::InterpreterOnly);
    assert!(oracle.ends_with("|a,b,c,d,e,f|0"), "{oracle}");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        assert_eq!(run_reserved_receiver(selection), oracle, "{selection:?}");
    }
}

const UNRESERVED_RECEIVER_SOURCE: &str = r#"
function Cell(v) {
  this.a = v; this.b = v + 1; this.c = v * 2; this.d = v - 1; this.e = v & 7; this.f = v | 1;
}
function Other() {}
Other.prototype.tag = "other";
function warm(rounds) {
  let acc = 0;
  for (let i = 0; i < rounds; i++) { const c = new Cell(i); acc += c.a + c.f; }
  return acc;
}
function foreign(rounds) {
  let acc = 0;
  let proto = "";
  for (let i = 0; i < rounds; i++) {
    const c = Reflect.construct(Cell, [i], Other);
    acc += c.a + c.b + c.c + c.d + c.e + c.f;
    proto = Object.getPrototypeOf(c).tag + "|" + Object.keys(c).join(",");
  }
  return acc + "|" + proto;
}
function runtime(rounds) {
  let acc = 0;
  for (let i = 0; i < rounds; i++) {
    const c = Reflect.construct(Cell, [i]);
    acc += c.a + c.b + c.c + c.d + c.e + c.f;
  }
  return acc + "|" + Object.keys(Reflect.construct(Cell, [1])).join(",");
}
warm(6000) + ";" + foreign(400) + ";" + runtime(400);
"#;

fn run_unreserved_receiver(selection: JitSelection) -> (String, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(UNRESERVED_RECEIVER_SOURCE.to_string()),
            "jit-unreserved-receiver-transitions.js",
        )
        .expect("constructor entered from every construct path")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats().jit_optimized_entries)
}

#[test]
fn constructor_field_transitions_prove_storage_for_any_receiver() {
    // `warm` promotes the constructor to the optimizing tier with baked field
    // transitions. `foreign` then constructs it through a `new.target` that
    // receiver preparation does not reserve for, and `runtime` through the
    // runtime construct boundary; the generated body must prove every slab
    // slot's storage (deopting to the canonical store that grows the slab)
    // rather than assume the caller reserved it.
    let (oracle, _) = run_unreserved_receiver(JitSelection::InterpreterOnly);
    assert!(oracle.contains("|other|a,b,c,d,e,f;"), "{oracle}");
    assert!(oracle.ends_with("|a,b,c,d,e,f"), "{oracle}");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, optimized_entries) = run_unreserved_receiver(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
        if selection == JitSelection::ProductionTiered {
            assert!(
                optimized_entries > 0,
                "the constructor must run on the optimizing tier for this proof"
            );
        }
    }
}
