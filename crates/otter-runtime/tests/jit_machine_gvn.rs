//! Effect-aware Machine GVN execution and artifact coverage.
//!
//! # Contents
//! - Interpreter/optimizing comparison for repeated scalar work and heap reads.
//! - Getter, Proxy, call, store, element, throw, and moving-allocation
//!   invalidation coverage from the shared differential corpus.
//! - Normalized Machine IR proof that a repeated decode and multiply collapse
//!   to one dominating instruction.
//!
//! # Invariants
//! - The corpus warms its functions from one script loop, which the optimizing
//!   tier OSR-compiles and inlines into; `duplicateDecode` is warmed again from
//!   short scripts ([`short_warm`]) so it compiles through its own entries.
//! - Observable reads are never reused across an invalidating effect.
//! - The optimized body publishes an explicit non-zero GVN/guard count.
//! - The surviving scalar value is used twice; there is no second lowering or
//!   emitter-local peephole.

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime, SourceInput,
};

#[path = "support/short_warm.rs"]
mod short_warm;

const SOURCE: &str = include_str!("../../otter-difftest/corpus/machine_step7_effects.js");
const MODULE: &str = "jit-machine-step7-effects.js";

fn run(selection: JitSelection) -> (String, Option<String>) {
    let builder = Runtime::builder().jit_selection(selection);
    let mut runtime = if selection == JitSelection::ProductionTiered {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("Step 7 runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(SOURCE), MODULE)
        .expect("Step 7 widening corpus");
    let completion = result.completion_string().to_owned();
    drop(result);
    let warm = short_warm::warm(
        &mut runtime,
        "duplicateDecode(warmRecord);",
        3000,
        "jit-machine-step7-duplicate-decode.js",
    );
    let ir = warm.bundles().find_map(|bundle| {
        let manifest = bundle.manifest();
        (manifest.module() == MODULE
            && manifest.function_name() == "duplicateDecode"
            && manifest.tier() == JitDebugTier::Optimizing)
            .then(|| {
                String::from_utf8(
                    bundle
                        .file(JitArtifactFileName::OptimizedIr)
                        .expect("duplicateDecode optimized IR")
                        .contents()
                        .to_vec(),
                )
                .expect("UTF-8 optimized IR")
            })
    });
    (completion, ir)
}

#[test]
fn effect_invalidation_stays_exact_while_redundant_scalar_guards_collapse() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, ir) = run(JitSelection::ProductionTiered);
    assert_eq!(compiled, oracle);
    assert_eq!(
        compiled,
        r#"{"accessor":[10,20],"getterCalls":2,"proxy":[8,9],"proxyGets":2,"call":[20,21],"callbackCalls":1,"store":[30,31],"element":[41,99],"commonArithmetic":0,"duplicateDecode":36,"thrown":"RangeError","throwGets":2}"#
    );

    let ir = ir.expect("duplicateDecode must reach Machine optimization");
    assert!(
        ir.contains("; gvn-eliminated=3 guards=1 loads=0"),
        "artifact must publish exact effect-aware GVN results:\n{ir}"
    );
    assert_eq!(ir.matches(" DecodeInt32 ").count(), 1, "{ir}");
    assert_eq!(ir.matches(" IntegerMul ").count(), 1, "{ir}");
    assert!(
        ir.contains("IntegerAdd [") && ir.contains("MachineValue(4)"),
        "the add must consume the one dominating product twice:\n{ir}"
    );
}
