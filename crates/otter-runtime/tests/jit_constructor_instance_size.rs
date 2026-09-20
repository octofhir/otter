//! Learned instance sizes for constructors that grow their receiver elsewhere.
//!
//! # Contents
//! - A constructor trampoline whose shared initializer adds seven fields,
//!   more than the receiver's inline words, constructed thousands of times.
//!
//! # Invariants
//! - After the first instances, every receiver the runtime prepares for the
//!   pair already holds storage for all seven fields, so the add-transition
//!   stores complete in generated code instead of growing the slab through
//!   the runtime store transition on every construct.
//! - Sibling constructor closures preserve independent observable layouts.
//! - Every tier returns the interpreter's completion.

use otter_runtime::{JitSelection, Runtime, RuntimeExecutionStats, SourceInput};

const SOURCE: &str = r#"
var Class = { create: function() { return function(a, b) { initialize(this, a, b); }; } };
var Info = Class.create();
function initialize(receiver, a, b) {
  receiver.isHit = false;
  receiver.hitCount = a;
  receiver.shape = null;
  receiver.position = b;
  receiver.normal = null;
  receiver.color = a + b;
  receiver.distance = a * 2;
};
function drive(rounds) {
  let acc = 0;
  let last = null;
  for (let i = 0; i < rounds; i++) {
    last = new Info(i, 1);
    acc += i;
  }
  return acc + "|" + last.hitCount + "," + last.position + "," + last.color + "," + last.distance;
}
drive(6000);
"#;

fn run(selection: JitSelection) -> (String, RuntimeExecutionStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-constructor-instance-size.js",
        )
        .expect("learned instance size")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats())
}

#[test]
fn receivers_are_reserved_for_the_fields_a_callee_adds() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, stats) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
        // Four of the seven fields lie beyond the inline words. Without
        // reservation each of them grows the slab through the runtime store
        // transition on every one of the thousands of generated constructs.
        assert!(
            stats.jit_runtime_property_stubs < 500,
            "{selection:?}: receivers must be pre-reserved for the callee's fields: {stats:?}"
        );
    }
}

#[test]
fn learned_capacity_survives_collection_after_samples_are_discarded() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("profile runtime");
        runtime
            .run_script(SourceInput::from_javascript(SOURCE), "profile-warmup.js")
            .expect("learned capacity warmup");
        runtime.force_gc().expect("discard observation handles");
        let before = runtime.execution_stats().jit_runtime_property_stubs;
        let result = runtime
            .run_script(
                SourceInput::from_javascript("drive(500);"),
                "profile-after-gc.js",
            )
            .expect("use scalar capacity after GC");
        assert_eq!(result.completion_string(), "124750|499,1,500,998");
        let transitions = runtime.execution_stats().jit_runtime_property_stubs - before;
        assert!(
            transitions < 50,
            "{selection:?}: GC must preserve learned capacity: {transitions} property transitions"
        );
    }
}

#[test]
fn sibling_constructor_profiles_preserve_live_instances() {
    let source = include_str!("../../otter-difftest/corpus/constructor_instance_profile.js")
        .replace("console.log(checksum);", "checksum;");
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source.clone()),
                "constructor-instance-profile.js",
            )
            .expect("independent constructor profiles");
        assert_eq!(result.completion_string(), "544000", "{selection:?}");
    }
}

#[test]
fn constructor_profiles_do_not_retain_the_last_instances_object_graph() {
    let mut retained = Vec::new();
    for closure in [false, true] {
        for selection in [
            JitSelection::InterpreterOnly,
            JitSelection::Template,
            JitSelection::ProductionTiered,
        ] {
            let constructor = if closure {
                "const Constructor = (function factory(marker) { return function Constructor() { this.payload = { marker }; }; })(42);"
            } else {
                "function Constructor() { this.payload = { marker: 42 }; }"
            };
            let source = format!(
                r#"
{constructor}
for (let warm = 0; warm < 256; warm++) new Constructor();
let instance = new Constructor();
const payloadWeak = new WeakRef(instance.payload);
const baselineWeak = new WeakRef({{ marker: 0 }});
instance = null;
undefined;
"#
            );
            let mut runtime = Runtime::builder()
                .jit_selection(selection)
                .build()
                .expect("profile runtime");
            runtime
                .run_script(SourceInput::from_javascript(source), "profile-retention.js")
                .expect("profile population");
            runtime
                .force_gc()
                .expect("collect unreachable receiver graph");
            let result = runtime
                .run_script(
                    SourceInput::from_javascript("[payloadWeak.deref() === undefined, baselineWeak.deref() === undefined].join(',');"),
                    "profile-retention-probe.js",
                )
                .expect("weak retention probe");
            assert!(
                result.completion_string().ends_with(",true"),
                "control WeakRef must clear after GC"
            );
            if result.completion_string() != "true,true" {
                retained.push(format!("closure={closure}, tier={selection:?}"));
            }
        }
    }
    assert!(
        retained.is_empty(),
        "constructor profiles retain dead object graphs: {retained:?}"
    );
}
