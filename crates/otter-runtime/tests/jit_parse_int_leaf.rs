//! Guarded static-native `parseInt(Int32)` leaf coverage.
//!
//! # Contents
//! - Exact int32 identity plus string, double, and object coercion misses.
//! - Bootstrap identity sharing and observable global replacement.
//! - Static-native lowering diagnostics and artifact structure, including an
//!   exact-arity rejection for an explicit radix.
//!
//! # Invariants
//! - Only the exact bootstrap callable with exactly one int32-tagged argument
//!   completes through the no-allocation leaf.
//! - Every guard miss precedes coercion or user code and resumes the canonical
//!   `parseInt` native exactly once.
//! - Generated code names the one shared runtime-stub declaration rather than
//!   a backend-private parse operation.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, SourceInput,
};
use otter_vm::{JitStaticNativeCallLoweringOutcome, JitStaticNativeCallLoweringRejectionReason};

const SEMANTIC_MATRIX: &str = r#"
function parseIdentity(value) {
  return parseInt(value);
}

function parseStringMiss(value) {
  return parseInt(value);
}

function parseDoubleMiss(value) {
  return parseInt(value);
}

function parseObjectMiss(value) {
  return parseInt(value);
}

for (let warm = 0; warm < 5000; warm++) {
  const value = (warm & 1023) - 512;
  parseIdentity(value);
  parseStringMiss(value);
  parseDoubleMiss(value);
  parseObjectMiss(value);
}

let objectCoercions = 0;
const coercingObject = {
  toString() {
    objectCoercions++;
    return "31";
  }
};

JSON.stringify({
  int32: [
    parseIdentity(-2147483648),
    parseIdentity(-1),
    parseIdentity(0),
    parseIdentity(2147483647)
  ],
  string: parseStringMiss("0x2a"),
  double: parseDoubleMiss(19.75),
  object: parseObjectMiss(coercingObject),
  objectCoercions
});
"#;

const IDENTITY_REPLACEMENT: &str = r#"
const bootstrapParseInt = parseInt;
const numberSharesIdentity = Number.parseInt === bootstrapParseInt;

function parseGlobal(value) {
  return parseInt(value);
}

for (let warm = 0; warm < 5000; warm++) {
  parseGlobal((warm & 255) - 128);
}

let replacementCalls = 0;
globalThis.parseInt = function replacement(value) {
  replacementCalls++;
  return value + 1000;
};

JSON.stringify({
  numberSharesIdentity,
  replacementResult: parseGlobal(7),
  replacementCalls,
  savedBootstrap: bootstrapParseInt(7),
  numberBootstrap: Number.parseInt(7)
});
"#;

const ARTIFACT_FIXTURE: &str = r#"
function parseIntLeafHot(value) {
  return parseInt(value);
}

function parseIntExplicitRadix(value, radix) {
  return parseInt(value, radix);
}

for (let warm = 0; warm < 5000; warm++) {
  parseIntLeafHot((warm & 1023) - 512);
}

// Enter the Template caller (50 calls) so arity rejection is observable.
for (let warm = 0; warm < 64; warm++) {
  parseIntExplicitRadix(17, 16);
}

JSON.stringify([
  parseIntLeafHot(41),
  parseIntExplicitRadix(17, 16)
]);
"#;

fn run(
    source: &'static str,
    selection: JitSelection,
) -> (String, otter_runtime::RuntimeExecutionStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        // Warm the called function itself instead of inlining it into top-level OSR.
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("parseInt runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(source),
            "jit-parse-int-leaf.js",
        )
        .expect("parseInt fixture");
    (result.completion_string().to_owned(), result.stats())
}

fn bundle_has_parse_int_leaf(bundle: &JitArtifactBundle) -> bool {
    let code_map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .expect("code-map artifact")
            .contents(),
    )
    .expect("valid code-map JSON");
    let has_region = code_map["regions"].as_array().is_some_and(|regions| {
        regions.iter().any(|region| {
            ((region["kind"] == "nativeLeafCall"
                && region["nativeLeafCall"] == "parse_int_i32_leaf")
                || region["kind"] == "machineNativeLeafCall")
                && region["bytePc"].is_u64()
        })
    });

    let relocations: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Relocations)
            .expect("relocation artifact")
            .contents(),
    )
    .expect("valid relocation JSON");
    let has_relocation = relocations["relocations"]
        .as_array()
        .is_some_and(|relocations| {
            relocations.iter().any(|relocation| {
                relocation["target"]["kind"] == "runtimeStub"
                    && relocation["target"]["name"] == "parse_int_i32_leaf"
            })
        });

    has_region && has_relocation
}

#[test]
fn int32_identity_and_all_tag_misses_match_the_interpreter() {
    let (oracle, _) = run(SEMANTIC_MATRIX, JitSelection::InterpreterOnly);
    let (compiled, stats) = run(SEMANTIC_MATRIX, JitSelection::ProductionTiered);

    assert_eq!(compiled, oracle);
    assert_eq!(
        oracle,
        r#"{"int32":[-2147483648,-1,0,2147483647],"string":42,"double":19,"object":31,"objectCoercions":1}"#
    );
    assert!(
        stats.jit_optimized_entries > 0,
        "fixture must execute generated parseInt callers: {stats:?}"
    );
}

#[test]
fn exact_bootstrap_identity_guards_observable_global_replacement() {
    let (oracle, _) = run(IDENTITY_REPLACEMENT, JitSelection::InterpreterOnly);
    let (compiled, stats) = run(IDENTITY_REPLACEMENT, JitSelection::ProductionTiered);

    assert_eq!(compiled, oracle);
    assert_eq!(
        oracle,
        r#"{"numberSharesIdentity":true,"replacementResult":1007,"replacementCalls":1,"savedBootstrap":7,"numberBootstrap":7}"#
    );
    assert!(
        stats.jit_optimized_entries > 0,
        "replacement must probe an installed generated caller: {stats:?}"
    );
}

#[test]
fn artifacts_and_events_expose_one_leaf_and_reject_explicit_radix() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(4)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("parseInt artifact runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(ARTIFACT_FIXTURE),
            "jit-parse-int-leaf-artifact.js",
        )
        .expect("parseInt artifact fixture");

    assert_eq!(result.completion_string(), "[41,23]");
    let report = result.jit_debug_report().expect("enabled JIT events");
    assert!(
        report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::StaticNativeCallLowered {
                tier: JitDebugTier::Optimizing,
                target: "parse_int_i32_leaf",
                outcome: JitStaticNativeCallLoweringOutcome::Generated,
                ..
            }
        )),
        "one-argument parseInt must lower through its declared leaf: {:?}",
        report.events()
    );
    assert!(
        report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::StaticNativeCallLowered {
                target: "parse_int_i32_leaf",
                outcome: JitStaticNativeCallLoweringOutcome::Rejected {
                    reason: JitStaticNativeCallLoweringRejectionReason::ArityUnsupported,
                },
                ..
            }
        )),
        "explicit radix must stay on the exact-arity canonical call path: {:?}",
        report.events()
    );

    let artifacts = result.jit_artifacts().expect("enabled JIT artifacts");
    assert!(
        artifacts.bundles().iter().any(|bundle| {
            bundle.manifest().tier() == JitDebugTier::Optimizing
                && bundle_has_parse_int_leaf(bundle)
        }),
        "an optimizing bundle must join the parseInt leaf region to its shared stub relocation: {artifacts:?}"
    );
}

#[test]
fn static_native_leaves_preserve_live_values_and_binary_arguments() {
    let source = r#"
const maximum = Math.max;
const minimum = Math.min;
const absolute = Math.abs;
const floor = Math.floor;
const squareRoot = Math.sqrt;
function leafPressure(value, a, b, c, d, e, f, g, object) {
  const parsed = parseInt(value);
  return parsed + maximum(a | 0, b | 0) + minimum(c | 0, d | 0) + absolute(e | 0)
      + floor(f) + squareRoot(g) + object.offset;
}
const kept = { offset: 100 };
for (let warm = 0; warm < 5000; warm++) {
  leafPressure(warm & 255, 2, 5, 3, 4, -7, 6, 81, kept);
}
let coercions = 0;
const value = { toString() { coercions++; return "41"; } };
JSON.stringify([
  leafPressure(41, 2, 5, 3, 4, -7, 6, 81, kept),
  leafPressure(value, 2, 5, 3, 4, -7, 6, 81, kept),
  leafPressure(41, 2, 5, 3, 4, -2147483648, 6, 81, kept),
  coercions,
  kept.offset
]);
"#;
    let (oracle, _) = run(source, JitSelection::InterpreterOnly);
    let (compiled, stats) = run(source, JitSelection::ProductionTiered);
    assert_eq!(oracle, "[171,171,2147483812,1,100]");
    assert_eq!(compiled, oracle);
    assert!(stats.jit_optimized_entries > 0, "{stats:?}");
}
