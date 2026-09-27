//! Proven local arguments reads across interpreter and generated activations.
//!
//! # Contents
//! - Exact Machine artifact admission and tier parity for local aliases.
//! - Cold materialization preserves getter mutations and the iterator intrinsic.
//!
//! # Invariants
//! - Optimized bodies must contain the native argument-window probe.
//! - Heap arguments remain valid through allocations in a live activation.
//!
//! # See also
//! - `otter-difftest/corpus/arguments_local_reads.js` covers escape and key cases.

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime, SourceInput,
};

#[test]
fn local_arguments_reads_use_machine_probes_and_preserve_cold_identity() {
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
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
            .unwrap();
        let result = runtime
            .run_script(SourceInput::from_javascript(source), "arguments-reads.js")
            .unwrap();
        assert_eq!(result.completion_string(), "1128750:47", "{selection:?}");
        if selection == JitSelection::ProductionTiered {
            let batch = result.jit_artifacts().unwrap();
            for name in ["sumArguments", "coldArguments"] {
                let bundle = batch
                    .bundles()
                    .iter()
                    .find(|bundle| {
                        bundle.manifest().function_name() == name
                            && bundle.manifest().tier() == JitDebugTier::Optimizing
                    })
                    .unwrap_or_else(|| panic!("missing Machine compilation of {name}"));
                let map = bundle.file(JitArtifactFileName::CodeMap).unwrap();
                assert!(
                    String::from_utf8_lossy(map.contents()).contains("machineArgumentsReadProbe"),
                    "{name}"
                );
            }
        }
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
