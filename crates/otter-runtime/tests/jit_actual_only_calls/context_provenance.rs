//! Captured `.call` targets across mapped and copied local contexts.
//!
//! # Contents
//! - Current generated linkage before and after a callable's own-property change.
//! - Per-iteration context copies keep captured call targets independent.
//! - Own caller/callee activations, single effects and young roots across collection.
//! - Optional setup reports and compile bundles for admission diagnosis.
//!
//! # Invariants
//! - Normal background compilation admits both exact current Graph generations.
//! - The intrinsic stays unchanged; the original generation owns IdentityGuard.
//! - Retrained calls retain actual Known machine linkage and never replay effects.
//!
//! # See also
//! - VM `jit_bake_tests::context_provenance` covers conservative context aliases.

use super::*;
use otter_vm::native_abi::{ExitAction, ExitReason};

const NAMES: [&str; 2] = ["provenanceMappedCaller", "provenanceCapturedLeaf"];
const CALL: &str = "provenanceMethod(7, provenancePayload);";
const LEAF_CALL: &str =
    "if (provenanceLeaf(7, provenancePayload) !== 12) throw 'leaf warm checksum';";
const LEAF_SOURCE: &str = r#"
function provenanceMakeLeaf(bias) {
  return function provenanceCapturedLeaf(value, payload) {
    if (arguments === payload) throw "distinct leaf arguments";
    payload.effects = payload.effects + 1;
    const child = actualRenewChild(payload);
    actualCollect(child);
    if (payload.child !== child) throw "preserved child identity";
    return value + bias + child.marker;
  };
}
"#;
const MAPPED_CALLER_SOURCE: &str = r#"function provenanceMakeCaller(callee) {
  return function provenanceMappedCaller(value, payload) {
    if (arguments === payload) throw "distinct caller arguments";
    payload.calls = payload.calls + 1;
    const actual = value;
    const actualPayload = payload;
    return callee.call(null, actual, actualPayload) + 1;
  };
}
"#;
const COPIED_CALLER_SOURCE: &str = r#"
function provenanceMakeCaller(callee) {
  function makeCopied(first, second, callee) {
    return function provenanceMappedCaller(value, payload) {
      for (let iteration = 0; iteration < value; iteration++) {
        if (arguments === payload) throw "distinct caller arguments";
        if (first + second !== 203) throw "captured anchors";
        payload.calls = payload.calls + 1;
        payload.iteration = () => iteration;
        const actual = value;
        const actualPayload = payload;
        return callee.call(null, actual, actualPayload) + 1;
      }
    };
  }
  return makeCopied(101, 102, callee);
}
"#;
const INSTANCE_SOURCE: &str = r#"const provenanceLeaf = provenanceMakeLeaf(3);
const provenanceMethod = provenanceMakeCaller(provenanceLeaf);
const provenancePayload = {marker: 2, calls: 0, effects: 0};
"#;
const CALLER_WARM: &str = r#"
for (let warm = 0; warm < 5000; warm++) {
  if (provenanceMethod(7, provenancePayload) !== 13) throw "warm checksum";
}
"#;

fn admit(fixture: &mut Fixture, names: &[&str], call: &str) {
    // Source-work admission is independent of GC verification speed. These
    // bounded ordinary batches fund both the leaf's initial compilation and
    // the caller's fresh post-deopt window; only worker settling uses a clock.
    let mut funding_batches = 0;
    let mut worker_wait = std::time::Duration::ZERO;
    for _ in 0..4096 {
        let pending = fixture.runtime.jit_code_residency().pending_compilations;
        if pending == 0
            && names.iter().all(|name| {
                fixture.current(name).is_some_and(|generation| {
                    generation.tier == NativeFrameKind::Optimizing
                        && generation.call_entry_offset.is_some()
                        && generation.active_count == 0
                })
            })
        {
            let before: Vec<_> = names
                .iter()
                .map(|name| fixture.current(name).unwrap())
                .collect();
            // Installation can precede the first use of a retrained lookup.
            // Ordinary calls must settle it before the isolated GC probe.
            fixture.setup(&call.repeat(16));
            if fixture.runtime.jit_code_residency().pending_compilations == 0
                && names.iter().zip(before).all(|(name, before)| {
                    fixture.current(name).is_some_and(|after| {
                        after.code_object_id == before.code_object_id
                            && after.generated_deopts == before.generated_deopts
                    })
                })
            {
                return;
            }
        }
        if pending == 0 {
            assert!(
                funding_batches < 128,
                "ordinary admission exhausted {funding_batches} batches: {:?}",
                fixture.runtime.jit_code_generation_snapshot()
            );
            fixture.setup(&call.repeat(32));
            funding_batches += 1;
        } else {
            assert!(
                worker_wait < std::time::Duration::from_secs(5),
                "worker settling exhausted after {funding_batches} batches: {:?}",
                fixture.runtime.jit_code_generation_snapshot()
            );
            let started = std::time::Instant::now();
            fixture.setup("0;");
            std::thread::yield_now();
            worker_wait += started.elapsed();
        }
    }
    panic!("bounded ordinary admission exhausted");
}

pub(super) fn record_setup(
    fixture: &Fixture,
    result: &otter_runtime::ExecutionResult,
    module: &str,
) {
    let Some(root) = std::env::var_os("OTTER_ACTUAL_SETUP_ARTIFACTS") else {
        return;
    };
    let root = std::path::PathBuf::from(root)
        .join(std::thread::current().name().unwrap())
        .join(format!("run-{:04}-{module}", fixture.setup_sequence));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("module.txt"), module).unwrap();
    std::fs::write(
        root.join("events.json"),
        serde_json::to_vec_pretty(result.jit_debug_report().unwrap()).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("current-generations.txt"),
        format!("{:#?}", fixture.runtime.jit_code_generation_snapshot()),
    )
    .unwrap();
    for bundle in result.jit_artifacts().unwrap().bundles() {
        let directory = root.join(format!("code-{}", bundle.manifest().code_object_id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec_pretty(bundle.manifest()).unwrap(),
        )
        .unwrap();
        for file in bundle.files() {
            std::fs::write(directory.join(file.name().as_str()), file.contents()).unwrap();
        }
    }
}

fn record_proof(fixture: &Fixture, phase: &str) {
    let Some(root) = std::env::var_os("OTTER_ACTUAL_PROOF_ARTIFACTS") else {
        return;
    };
    let root = std::path::PathBuf::from(root).join(format!("context-provenance-{phase}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("current-generations.txt"),
        format!("{:#?}", fixture.runtime.jit_code_generation_snapshot()),
    )
    .unwrap();
    for name in NAMES {
        let current = fixture.current(name).unwrap();
        let bundle = &fixture.artifacts[&current.code_object_id];
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec_pretty(bundle.manifest()).unwrap(),
        )
        .unwrap();
        for file in bundle.files() {
            std::fs::write(directory.join(file.name().as_str()), file.contents()).unwrap();
        }
    }
}

fn assert_current_edge(fixture: &Fixture, copied: bool) {
    fixture.edge(NAMES[0], NAMES[1]);
    let caller = fixture.current(NAMES[0]).unwrap();
    let callee = fixture.current(NAMES[1]).unwrap();
    let bundle = &fixture.artifacts[&caller.code_object_id];
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    if copied {
        let map: serde_json::Value = serde_json::from_slice(
            bundle
                .file(JitArtifactFileName::CodeMap)
                .unwrap()
                .contents(),
        )
        .unwrap();
        assert!(
            map["regions"].as_array().unwrap().iter().any(|region| {
                region["functionId"] == caller.function_id
                    && region["operation"]
                        .as_str()
                        .is_some_and(|op| op.contains("CopyContext"))
            }),
            "the exact own caller emits its per-iteration context copy"
        );
    }
    let calls: Vec<_> = ir.lines().filter(|line| line.contains("CallJs")).collect();
    assert_eq!(calls.len(), 1, "one generated captured call: {ir}");
    assert!(
        calls[0].contains(&format!(
            "plan: Bytecode(JitDirectCallPlan {{ function_id: {}, code_object_id: {},",
            callee.function_id, callee.code_object_id
        )),
        "the plan names the exact current leaf generation: {}",
        calls[0]
    );
}

fn assert_retained_iteration(fixture: &mut Fixture) {
    let retained = fixture.run(
        "JSON.stringify([provenancePayload.iteration(), provenancePayload.calls, provenancePayload.effects]);",
        "copied-context-retained.js",
    );
    assert_eq!(retained.completion_string(), "[0,1,1]");
}

#[test]
fn mapped_context_keeps_captured_call_linkage_after_identity_retraining_and_gc() {
    captured_call_linkage_after_identity_retraining_and_gc(MAPPED_CALLER_SOURCE, false);
}

#[test]
fn copied_context_keeps_captured_call_linkage_after_identity_retraining_and_gc() {
    captured_call_linkage_after_identity_retraining_and_gc(COPIED_CALLER_SOURCE, true);
}

fn captured_call_linkage_after_identity_retraining_and_gc(caller_source: &str, copied: bool) {
    let mut fixture = Fixture::unwarmed_in_realm(JitSelection::ProductionTiered, false);
    let source_name = "actual-only-setup.js";
    fixture.setup(&format!("{LEAF_SOURCE}{caller_source}{INSTANCE_SOURCE}"));
    if copied {
        // Establish the closure slot before leaf admission, preserving its
        // receiver shape while the copied caller replaces this slot's value.
        fixture.setup("provenancePayload.iteration = undefined;");
    }
    // Plans retain a capture-time generation stamp while dispatch follows the
    // stable function entry cell. Admit the leaf before compiling its caller so
    // the exact-current stamp assertion is independent of worker completion order.
    admit(&mut fixture, &NAMES[1..], LEAF_CALL);
    let prepared_leaf = fixture.current(NAMES[1]).unwrap();
    fixture.setup(CALLER_WARM);
    admit(&mut fixture, &NAMES, CALL);
    assert_eq!(
        fixture.current(NAMES[1]).unwrap().code_object_id,
        prepared_leaf.code_object_id
    );
    record_proof(&fixture, if copied { "copied-before" } else { "before" });
    assert_current_edge(&fixture, copied);
    let original = fixture.current(NAMES[0]).unwrap();
    let leaf = fixture.current(NAMES[1]).unwrap();
    fixture.probe(
        "provenancePayload.calls = 0; provenancePayload.effects = 0; JSON.stringify([provenanceMethod(7, provenancePayload), provenancePayload.calls, provenancePayload.effects]);",
        &NAMES,
        "[13,1,1]",
    );

    let result = fixture.run(
        r#"
provenancePayload.calls = 0; provenancePayload.effects = 0;
provenanceLeaf.diagnosticOwnProperty = 1;
if (provenanceLeaf.call !== Function.prototype.call) throw "intrinsic changed";
JSON.stringify([provenanceMethod(7, provenancePayload), provenancePayload.calls, provenancePayload.effects]);
"#,
        "context-provenance-invalidate.js",
    );
    record_setup(&fixture, &result, "context-provenance-invalidate.js");
    assert_eq!(result.completion_string(), "[13,1,1]");
    let report = result.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let exits: Vec<_> = report
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                JitDebugEvent::Bail { .. }
                    | JitDebugEvent::EnteredGenerationDeopt { .. }
                    | JitDebugEvent::InlineDeoptFrame { .. }
            )
        })
        .collect();
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert!(
        matches!(exits[0], JitDebugEvent::EnteredGenerationDeopt {
            callee_function_id, callee_code_object_id,
            exit_reason: ExitReason::IdentityGuard, exit_action: ExitAction::Recompile, ..
        } if *callee_function_id == original.function_id && *callee_code_object_id == original.code_object_id),
        "the original mapped caller owns the intrinsic identity exit: {exits:?}"
    );
    assert_eq!(
        fixture.current(NAMES[1]).unwrap().code_object_id,
        leaf.code_object_id
    );

    admit(&mut fixture, &NAMES, CALL);
    let retrained = fixture.current(NAMES[0]).unwrap();
    assert_ne!(retrained.code_object_id, original.code_object_id);
    record_proof(
        &fixture,
        if copied {
            "copied-retrained"
        } else {
            "retrained"
        },
    );
    assert_current_edge(&fixture, copied);

    fixture.collect.store(true, Ordering::Relaxed);
    for phase in 0..2 {
        if phase != 0 {
            fixture.runtime.force_gc().unwrap();
            if copied {
                // Read the previous escaped context before the next probe
                // replaces it, proving standalone full-collection retention.
                assert_retained_iteration(&mut fixture);
            }
        }
        fixture.probe(
            "provenancePayload.calls = 0; provenancePayload.effects = 0; JSON.stringify([provenanceMethod(7, provenancePayload), provenancePayload.calls, provenancePayload.effects]);",
            &NAMES,
            "[13,1,1]",
        );
        if copied {
            assert_retained_iteration(&mut fixture);
        }
        assert_eq!(fixture.collections.load(Ordering::Relaxed), phase + 1);
        assert_eq!(
            fixture.current(NAMES[0]).unwrap().code_object_id,
            retrained.code_object_id
        );
        assert_eq!(
            fixture.current(NAMES[1]).unwrap().code_object_id,
            leaf.code_object_id
        );
    }
    let observations = fixture.observations.lock().unwrap();
    assert_eq!(observations.len(), 2);
    for observation in observations.iter() {
        let (children, before, after) = observation.as_ref().expect("native root observation");
        assert_eq!(children.len(), 1);
        assert_ne!(
            children[0].before, children[0].after,
            "the actual young carrier moves"
        );
        assert_eq!(children[0].marker_before, children[0].marker_after);
        let before: serde_json::Value = serde_json::from_str(before).unwrap();
        let after: serde_json::Value = serde_json::from_str(after).unwrap();
        assert_eq!(
            before, after,
            "exact suspended native sources survive collection"
        );
        let frames = before.as_array().unwrap();
        for (name, line) in [
            (
                NAMES[0],
                "return callee.call(null, actual, actualPayload) + 1;",
            ),
            (NAMES[1], "actualCollect(child);"),
        ] {
            let matching: Vec<_> = frames
                .iter()
                .filter(|frame| frame["functionName"] == name)
                .collect();
            assert_eq!(matching.len(), 1, "one own activation for {name}");
            assert_eq!(matching[0]["scriptName"], source_name);
            assert_eq!(matching[0]["sourceLine"].as_str().unwrap().trim(), line);
        }
    }
    assert_current_edge(&fixture, copied);
}
