//! Machine partial-escape execution and artifact coverage.
//!
//! # Contents
//! - Interpreter/optimizing comparison for identity and escape boundaries.
//! - WeakRef, exceptions, nested deopt, OSR, aliasing, and moving-GC coverage.
//! - Artifact proof that fixed arrays virtualize and materialize through the
//!   authoritative Machine frame-state contract.
//!
//! # Invariants
//! - Virtual identity is never externally observable before one materialization.
//! - A deopt, throw, safepoint, capture, or external call receives a real object.
//! - The pass reports eliminated allocations without an emitter-local shortcut.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime, SourceInput,
};

const SOURCE: &str = include_str!("../../otter-difftest/corpus/machine_step9_pea.js");
const MODULE: &str = "jit-machine-step9-pea.js";

fn run(selection: JitSelection) -> (String, Vec<String>) {
    let builder = Runtime::builder().jit_selection(selection);
    let mut runtime = if selection == JitSelection::ProductionTiered {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("Step 9 runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(SOURCE), MODULE)
        .expect("Step 9 widening corpus");
    let completion = result.completion_string().to_owned();
    let ir = result
        .jit_artifacts()
        .into_iter()
        .flat_map(|batch| batch.bundles())
        .filter(|bundle| {
            bundle.manifest().module() == MODULE
                && bundle.manifest().tier() == JitDebugTier::Optimizing
        })
        .filter_map(|bundle| {
            bundle.file(JitArtifactFileName::OptimizedIr).map(|file| {
                String::from_utf8(file.contents().to_vec()).expect("UTF-8 optimized IR")
            })
        })
        .collect();
    runtime.force_gc().expect("Step 9 roots unlink after run");
    (completion, ir)
}

#[test]
fn virtual_objects_materialize_once_at_every_observable_boundary() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, artifacts) = run(JitSelection::ProductionTiered);
    assert_eq!(compiled, oracle);
    assert!(
        artifacts
            .iter()
            .any(|ir| ir.contains("pea-virtualized=") && !ir.contains("pea-virtualized=0")),
        "at least one fixed literal must virtualize: {artifacts:#?}"
    );
    assert!(
        artifacts
            .iter()
            .any(|ir| ir.contains("pea-materialized=") && !ir.contains("pea-materialized=0")),
        "an escaping virtual literal must materialize: {artifacts:#?}"
    );
    assert!(
        artifacts
            .iter()
            .any(|ir| { ir.contains("pea-eliminated=") && !ir.contains("pea-eliminated=0") }),
        "a local fixed literal allocation must disappear: {artifacts:#?}"
    );
}
