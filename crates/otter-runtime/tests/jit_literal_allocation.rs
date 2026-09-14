//! Compiled literal allocation through the shared boxed-value span boundary.
//!
//! # Contents
//! - Empty, sparse and dense literals with aliased moving references.
//! - Dense encoding limit and incremental construction above that limit.
//! - Machine allocation with unrelated live values and mixed tagged elements.
//!
//! # Invariants
//! - Compiled results match the interpreter and allocation does not turn a
//!   hole into an own undefined element or lose a previously allocated value.
//! - Artifacts must prove both literal boundaries use the fixed value ABI.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn machine_literal_spans_publish_live_roots_and_preserve_holes() {
    let source = r#"
function allocate(input) {
  const empty = {};
  const array = [input, , empty, 41, 1.5];
  return [empty, array, input];
}
const input = { tag: 42 };
for (let warm = 0; warm < 5000; warm++) allocate(input);
const result = allocate(input);
for (let warm = 0; warm < 32; warm++) allocate(input);
JSON.stringify([
  result[2] === input, result[1][0] === input, result[1][2] === result[0],
  result[1].length, 1 in result[1], result[1][3], result[1][4],
  Object.getPrototypeOf(result[0]) === Object.prototype, result[2].tag
]);
"#;
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
            .expect("Machine literal runtime");
        let result = runtime
            .run_script(SourceInput::from_javascript(source), "machine-literals.js")
            .expect("Machine literal allocation");
        assert_eq!(
            result.completion_string(),
            "[true,true,true,5,false,41,1.5,true,42]"
        );
        if selection == JitSelection::ProductionTiered {
            assert!(
                result
                    .jit_artifacts()
                    .is_some_and(|batch| batch.bundles().iter().any(|bundle| {
                        bundle.manifest().function_name() == "allocate"
                            && bundle.manifest().tier() == otter_runtime::JitDebugTier::Optimizing
                            && bundle
                                .file(JitArtifactFileName::CodeMap)
                                .is_some_and(|file| {
                                    let map: serde_json::Value =
                                        serde_json::from_slice(file.contents())
                                            .expect("Machine code map");
                                    map["regions"]
                                        .as_array()
                                        .expect("Machine regions")
                                        .iter()
                                        .filter(|region| {
                                            region["kind"] == "machineLiteralAllocation"
                                        })
                                        .count()
                                        == 3
                                })
                    })),
                "literal fixture must compile through Machine: {:?}",
                result.jit_debug_report()
            );
        }
        runtime
            .force_gc()
            .expect("literal roots must unlink on return");
    }
}

#[test]
fn template_literal_spans_preserve_holes_aliases_and_dense_limits() {
    for count in [0, 240, 241] {
        let elements = std::iter::repeat_n("object", count)
            .collect::<Vec<_>>()
            .join(",");
        let source = format!(
            r#"
function literals(value) {{
  const object = {{ value }};
  const sparse = [object, , object];
  const dense = [{elements}];
  const empty = [];
  const ordinary = {{}};
  return [object, sparse, dense, empty, ordinary];
}}
let kept = literals(41);
for (let warm = 0; warm < 256; warm++) literals(warm);
JSON.stringify([
  kept[0].value, kept[1].length, 1 in kept[1], kept[1][0] === kept[1][2],
  kept[2].length, kept[2].every(value => value === kept[0]), kept[3].length,
  Object.getPrototypeOf(kept[4]) === Object.prototype
]);
"#
        );
        let mut oracle = None;
        for selection in [JitSelection::InterpreterOnly, JitSelection::Template] {
            let mut runtime = Runtime::builder()
                .jit_selection(selection)
                .jit_debug(JitDebugRequest::artifacts())
                .build()
                .expect("literal runtime");
            let result = runtime
                .run_script(SourceInput::from_javascript(&source), "literal-spans.js")
                .expect("literal allocation");
            let expected = format!("[41,3,false,true,{count},true,0,true]");
            assert_eq!(result.completion_string(), expected);
            if selection == JitSelection::InterpreterOnly {
                oracle = Some(result.completion_string().to_owned());
                continue;
            }
            assert_eq!(Some(result.completion_string()), oracle.as_deref());
            let batch = result.jit_artifacts().expect("compiled literal artifacts");
            for id in [
                otter_vm::native_abi::STUB_JIT_NEW_OBJECT.id,
                otter_vm::native_abi::STUB_JIT_NEW_ARRAY.id,
            ] {
                assert!(
                    batch
                        .bundles()
                        .iter()
                        .filter(|bundle| bundle.manifest().function_name() == "literals")
                        .any(
                            |bundle| bundle.file(JitArtifactFileName::Relocations).is_some_and(
                                |file| {
                                    let json: serde_json::Value =
                                        serde_json::from_slice(file.contents())
                                            .expect("relocation JSON");
                                    json["relocations"]
                                        .as_array()
                                        .expect("relocation rows")
                                        .iter()
                                        .any(|row| {
                                            row["target"]["id"] == id
                                                && row["target"]["signature"]
                                                    == "reentrantValueSpan"
                                        })
                                }
                            )
                        ),
                    "literal stub {id} must use boxed values"
                );
            }
        }
    }
}
