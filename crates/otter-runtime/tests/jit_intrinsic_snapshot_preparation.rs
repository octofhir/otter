//! Collection property preparation preserves previously observed method hits.
//!
//! # Contents
//! - Mixed and nested method/property snapshots retain the Template leaf call.
//! - A later property-only compilation cannot stale an installed method guard.
//!
//! # Invariants
//! - Each fixture starts with untouched realm collection prototypes.
//! - Hot probes preserve semantics without property crossings or recompilation.
//! - AArch64 Template retains its existing allocation-free native leaf boundary.

use otter_runtime::{
    ExecutionResult, JitDebugRequest, JitDebugTier, JitSelection, Runtime, RuntimeExecutionStats,
    SourceInput,
};

const TIERS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("collection snapshot runtime")
}

fn run(runtime: &mut Runtime, source: &str) -> ExecutionResult {
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "collection-snapshot.js",
        )
        .expect("collection snapshot fixture")
}

fn assert_compiled(result: &ExecutionResult, function: &str, selection: JitSelection) {
    let tier = match selection {
        JitSelection::Template => JitDebugTier::Template,
        JitSelection::ProductionTiered => JitDebugTier::Optimizing,
        JitSelection::InterpreterOnly => unreachable!("interpreter fixture does not compile"),
    };
    assert!(
        result
            .jit_artifacts()
            .expect("enabled artifacts")
            .bundles()
            .iter()
            .any(|bundle| bundle.manifest().tier() == tier
                && bundle.manifest().function_name() == function),
        "{function} must compile before the hot probe: {:?}",
        result.jit_artifacts().map(|batch| batch
            .bundles()
            .iter()
            .map(|bundle| (bundle.manifest().tier(), bundle.manifest().function_name()))
            .collect::<Vec<_>>())
    );
}

fn assert_hot(
    before: RuntimeExecutionStats,
    after: RuntimeExecutionStats,
    selection: JitSelection,
) {
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    if selection == JitSelection::InterpreterOnly {
        return;
    }
    assert_eq!(
        after.jit_runtime_property_stubs, before.jit_runtime_property_stubs,
        "{selection:?}: the prepared collection property must remain generated"
    );
    #[cfg(target_arch = "aarch64")]
    if selection == JitSelection::Template {
        assert_eq!(
            after.jit_to_rust_call_transitions, before.jit_to_rust_call_transitions,
            "shape preparation must not turn the existing collection leaf into a full call"
        );
    }
}

#[test]
fn mixed_and_nested_collection_snapshots_preserve_method_leaf() {
    for selection in TIERS {
        for property in ["receiver.set", "loadSet(receiver)"] {
            let mut runtime = runtime(selection);
            let setup = run(
                &mut runtime,
                &format!(
                    r#"
var table = new Map([[0, 7]]);
var argumentsList = [table];
var observed;
function loadSet(receiver) {{ return receiver.set; }}
function mixed(receiver) {{
    for (var index = 0; index < 3; index++) {{}}
    observed = {property};
    return receiver.get(0);
}}
for (var warm = 0; warm < 4010; warm++) Reflect.apply(mixed, undefined, argumentsList);
"#
                ),
            );
            if selection != JitSelection::InterpreterOnly {
                assert_compiled(&setup, "mixed", selection);
            }
            let before = runtime.execution_stats();
            assert_eq!(run(&mut runtime, "mixed(table);").completion_string(), "7");
            assert_hot(before, runtime.execution_stats(), selection);
            assert_eq!(
                run(&mut runtime, "typeof observed;").completion_string(),
                "function"
            );
        }
    }
}

#[test]
fn later_property_compilation_preserves_an_installed_method_leaf() {
    for selection in TIERS {
        let mut runtime = runtime(selection);
        let method = run(
            &mut runtime,
            r#"
var table = new Map([[0, 7]]);
var argumentsList = [table];
function methodOnly(receiver) {
    for (var index = 0; index < 3; index++) {}
    return receiver.get(0);
}
for (var warm = 0; warm < 4010; warm++) Reflect.apply(methodOnly, undefined, argumentsList);
"#,
        );
        if selection != JitSelection::InterpreterOnly {
            assert_compiled(&method, "methodOnly", selection);
        }
        let before = runtime.execution_stats();
        assert_eq!(
            run(&mut runtime, "methodOnly(table);").completion_string(),
            "7"
        );
        assert_hot(before, runtime.execution_stats(), selection);

        let property = run(
            &mut runtime,
            r#"
function propertyOnly(receiver) {
    for (var index = 0; index < 3; index++) {}
    return receiver.set;
}
for (var warm = 0; warm < 4010; warm++) Reflect.apply(propertyOnly, undefined, argumentsList);
"#,
        );
        if selection != JitSelection::InterpreterOnly {
            assert_compiled(&property, "propertyOnly", selection);
        }
        let before = runtime.execution_stats();
        let probe = run(&mut runtime, "methodOnly(table);");
        assert_eq!(probe.completion_string(), "7");
        assert_hot(before, runtime.execution_stats(), selection);
        if selection != JitSelection::InterpreterOnly {
            assert!(
                probe
                    .jit_artifacts()
                    .expect("enabled artifacts")
                    .bundles()
                    .is_empty(),
                "the original installed method must remain usable without recompilation"
            );
        }
    }
}
