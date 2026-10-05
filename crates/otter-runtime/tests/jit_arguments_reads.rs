//! Proven local arguments reads across interpreter and generated activations.
//!
//! # Contents
//! - Tier parity for local aliases across interpreter, Template and
//!   production tiering.
//! - Cold materialization preserves getter mutations and the iterator intrinsic.
//!
//! # Invariants
//! - Every tier produces the interpreter's result for the same source.
//! - Heap arguments remain valid through allocations in a live activation.
//!
//! # See also
//! - `otter-difftest/corpus/arguments_local_reads.js` covers escape and key cases.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn local_arguments_reads_keep_tier_parity_and_preserve_cold_identity() {
    let source = r#"
function sumArguments() {
    var a = arguments, sum = 0;
    for (var i = 0; i < a.length; ++i) sum += a[i].n;
    return sum;
}
var coldKey = -1;
function coldArguments() {
    var a = arguments;
    var first = a[coldKey];
    return first + a[0].n + a.length;
}
Object.defineProperty(Object.prototype, '-1', {
    configurable: true,
    get: function() { this[0] = { n: 31 }; this.length = 9; return 7; }
});
var sum = 0, cold;
for (var i = 0; i < 1500; ++i) {
    sum += sumArguments({ n: i }, { n: 3 });
    cold = coldArguments({ n: i });
}
delete Object.prototype['-1'];
sum + ':' + cold;
"#;
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(SourceInput::from_javascript(source), "arguments-reads.js")
            .unwrap();
        assert_eq!(result.completion_string(), "1128750:47", "{selection:?}");
    }
}

#[test]
fn restored_arguments_keep_the_original_iterator_intrinsic() {
    let source = Runtime::builder().build().unwrap();
    let snapshot = source.capture_isolate_snapshot().unwrap();
    let mut restored = Runtime::from_isolate_snapshot(&snapshot).unwrap();
    restored.force_gc().unwrap();
    let result = restored
        .run_script(
            SourceInput::from_javascript(
                r#"
        var original = Array.prototype.values;
        Array.prototype.values = function replacement() {};
        function readIterator() { return arguments[Symbol.iterator] === original; }
        readIterator(1, 2);
    "#,
            ),
            "restored-arguments.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "true");
}
