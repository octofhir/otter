//! Runtime-boundary coverage for owned, default-off JIT debug events.
//!
//! # Contents
//! - Default-off behavior and ordered template compile events.
//! - Optimizing OSR consumes unary feedback without an immediate side exit.
//! - Eligible numeric methods splice into guarded optimizing-tier bodies.
//! - Abrupt completion followed by explicit report draining.
//! - Report ownership across full GC, later allocation, and nested JIT entry.
//! - Async [`otter_runtime::Otter`] event-loop success and abrupt-failure
//!   transport.
//! - Concurrent handle commands keep their event batches isolated.
//!
//! # Invariants
//! - Enabling diagnostics never changes script completion.
//! - Reports contain owned data and do not retain VM or GC handles.
//! - A compile-prepared event precedes its matching compile-finished event.
//!
//! # See also
//! - `otter_vm::jit_debug` for the current event shape and isolate-local state.

use std::collections::BTreeSet;

use otter_runtime::JitDebugCompileOutcome;
use otter_runtime::{
    JitDebugEvent, JitDebugRequest, JitDebugTarget, JitDebugTier, JitSelection, Otter, Runtime,
    SourceInput,
};

const HOT_LOOP: &str = r#"
let total = 0;
for (let i = 0; i < 2000; i++) {
  total += i;
}
total;
"#;

const NESTED_JIT: &str = r#"
function inner(limit) {
  let total = 0;
  for (let i = 0; i < limit; i++) {
    total += i;
  }
  return total;
}

function outer(limit) {
  let total = 0;
  for (let i = 0; i < 600; i++) {
    total += inner(limit + i);
  }
  return total;
}

outer(48);
"#;

fn runtime_with_events() -> Runtime {
    Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("runtime with JIT debug events")
}

fn assert_ordered_template_compile(events: &[JitDebugEvent]) {
    let prepared = events.iter().enumerate().find_map(|(index, event)| {
        let JitDebugEvent::CompilePrepared {
            function_id,
            tier: JitDebugTier::Template,
            target,
            ..
        } = event
        else {
            return None;
        };
        Some((index, *function_id, *target))
    });
    let (prepared_index, function_id, target) = prepared.expect("template compile-prepared event");
    let finished_index = events
        .iter()
        .enumerate()
        .skip(prepared_index + 1)
        .find_map(|(index, event)| match event {
            JitDebugEvent::CompileFinished {
                function_id: finished_function_id,
                tier: JitDebugTier::Template,
                target: finished_target,
                ..
            } if *finished_function_id == function_id && *finished_target == target => Some(index),
            _ => None,
        })
        .expect("matching template compile-finished event");

    assert!(
        prepared_index < finished_index,
        "compile preparation must precede its result"
    );
}

#[test]
fn jit_debug_reports_are_default_off() {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::Template)
        .build()
        .expect("default runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(HOT_LOOP),
            "jit-debug-default-off.js",
        )
        .expect("hot loop");

    assert_eq!(result.completion_string(), "1999000");
    assert!(result.jit_debug_report().is_none());
    assert!(result.jit_artifacts().is_none());
    assert!(runtime.take_jit_debug_report().is_none());
    assert!(runtime.take_jit_artifacts().is_none());
}

#[test]
fn template_osr_emits_ordered_compile_events() {
    let mut runtime = runtime_with_events();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(HOT_LOOP),
            "jit-debug-template-osr.js",
        )
        .expect("hot loop");
    let report = result
        .jit_debug_report()
        .expect("enabled run owns a report");

    assert_eq!(result.completion_string(), "1999000");
    assert_ordered_template_compile(report.events());
    let (bytecode_instruction_count, source_work, exit_count) = report
        .events()
        .iter()
        .find_map(|event| match event {
            JitDebugEvent::CompilePrepared {
                tier: JitDebugTier::Template,
                target: JitDebugTarget::Osr { .. },
                bytecode_instruction_count,
                source_work,
                exit_count,
                ..
            } => Some((*bytecode_instruction_count, *source_work, *exit_count)),
            _ => None,
        })
        .expect("template compile inputs");
    assert!(bytecode_instruction_count > 0);
    assert!(
        source_work > 0,
        "the script root's own loop work requests OSR without a call boundary"
    );
    assert_eq!(exit_count, 0);
    assert!(
        report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::CompilePrepared {
                tier: JitDebugTier::Template,
                target: JitDebugTarget::Osr { .. },
                ..
            }
        )),
        "cost-qualified loop must exercise the template OSR request"
    );
    #[cfg(target_arch = "aarch64")]
    {
        let (compile_started_ns, queue_delay_ns, compile_duration_ns, ir_node_count, code_bytes) =
            report
                .events()
                .iter()
                .find_map(|event| match event {
                    JitDebugEvent::CompileFinished {
                        tier: JitDebugTier::Template,
                        target: JitDebugTarget::Osr { .. },
                        compile_started_ns,
                        queue_delay_ns,
                        compile_duration_ns,
                        ir_node_count,
                        outcome: JitDebugCompileOutcome::Compiled { code_bytes, .. },
                        ..
                    } => Some((
                        *compile_started_ns,
                        *queue_delay_ns,
                        *compile_duration_ns,
                        *ir_node_count,
                        *code_bytes,
                    )),
                    _ => None,
                })
                .expect("template compile telemetry");
        let compile_started_ns = compile_started_ns.expect("compiled hook entered");
        assert!(compile_started_ns > 0);
        assert!(queue_delay_ns <= compile_started_ns);
        assert!(compile_duration_ns > 0);
        assert!(ir_node_count > 0);
        assert!(code_bytes > 0);
    }
}

#[test]
fn extracted_int32_math_call_publishes_one_stable_optimizing_body() {
    let artifact_directory =
        std::env::var_os("OTTER_STATIC_NATIVE_ARTIFACTS").map(std::path::PathBuf::from);
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("production-tiered runtime with events");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
(function () {
  const target = Math.abs;
  let sum = 0;
  for (let i = 0; i < 5000; i = i + 1) {
    sum = sum + target(-1);
  }
  return sum;
})();
"#,
            ),
            "jit-debug-unary-feedback.js",
        )
        .expect("extracted native call loop");
    let report = result
        .jit_debug_report()
        .expect("enabled run owns a report");

    if let Some(directory) = artifact_directory {
        std::fs::create_dir(&directory).expect("fresh static-native diagnostic directory");
        std::fs::write(
            directory.join("events.json"),
            serde_json::to_vec_pretty(report).expect("owned event report"),
        )
        .expect("write event report");
        std::fs::write(directory.join("completion.txt"), result.completion_string())
            .expect("write completion");
        std::fs::write(
            directory.join("current-generations.txt"),
            format!("{:#?}", runtime.jit_code_generation_snapshot()),
        )
        .expect("write generation snapshot");
        let artifacts = result.jit_artifacts().expect("requested compile artifacts");
        for bundle in artifacts.bundles() {
            let code = directory.join(format!("code-{}", bundle.manifest().code_object_id()));
            std::fs::create_dir(&code).expect("fresh code bundle directory");
            std::fs::write(
                code.join("manifest.json"),
                serde_json::to_vec_pretty(bundle.manifest()).expect("owned bundle manifest"),
            )
            .expect("write bundle manifest");
            for file in bundle.files() {
                std::fs::write(code.join(file.name().as_str()), file.contents())
                    .expect("write owned compile artifact");
            }
        }
    }

    assert_eq!(result.completion_string(), "5000");
    let optimizing_compiles = report
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                JitDebugEvent::CompileFinished {
                    tier: JitDebugTier::Optimizing,
                    outcome: JitDebugCompileOutcome::Compiled { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        optimizing_compiles,
        1,
        "guarded Int32 Math lowering must not need feedback-refresh recompilation: {:?}",
        report.events()
    );
    let optimizing_bails = report
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                JitDebugEvent::Bail {
                    tier: JitDebugTier::Optimizing,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        optimizing_bails,
        0,
        "guarded Int32 Math lowering must avoid the warmup leaf bail: {:?}",
        report.events()
    );
    assert!(!report.truncated());
    let (function_id, code_object_id) = report
        .events()
        .iter()
        .find_map(|event| match event {
            JitDebugEvent::CompileFinished {
                function_id,
                tier: JitDebugTier::Optimizing,
                outcome: JitDebugCompileOutcome::Compiled { code_object_id, .. },
                ..
            } => Some((*function_id, *code_object_id)),
            _ => None,
        })
        .expect("the sole optimizing generation");
    let call_pc = report
        .events()
        .iter()
        .find_map(|event| match event {
            JitDebugEvent::StaticNativeCallPlan {
                caller_function_id,
                instruction_pc,
                tier: JitDebugTier::Optimizing,
                target: "math_abs_leaf",
            } if *caller_function_id == function_id => Some(*instruction_pc),
            _ => None,
        })
        .expect("the extracted Math.abs call plan");
    assert!(
        runtime
            .jit_code_generation_snapshot()
            .iter()
            .any(|generation| {
                generation.function_id == function_id
                    && generation.code_object_id == code_object_id
                    && generation.current_entry
                    && generation.linked
            })
    );
    let artifacts = result
        .jit_artifacts()
        .expect("emitted optimizing artifacts");
    assert!(!artifacts.truncated());
    let bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| bundle.manifest().code_object_id() == code_object_id)
        .expect("the exact compiled generation's artifact");
    assert_eq!(bundle.manifest().function_id(), function_id);
    assert_eq!(bundle.manifest().tier(), JitDebugTier::Optimizing);
    let code_map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(otter_runtime::JitArtifactFileName::CodeMap)
            .expect("emitted code map")
            .contents(),
    )
    .expect("valid code map");
    let call_regions: Vec<_> = code_map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["functionId"] == function_id
                && region["logicalPc"] == call_pc
        })
        .collect();
    // The optimizing tier reduces Math.abs to arithmetic after its identity
    // guard. A leaf-call event would claim a helper that this body does not emit.
    assert_eq!(call_regions.len(), 2, "{call_regions:?}");
    let guard = call_regions
        .iter()
        .find(|region| {
            region["operation"]
                .as_str()
                .unwrap()
                .contains(" CheckNative(")
        })
        .expect("emitted exact native identity guard");
    let math = call_regions
        .iter()
        .find(|region| {
            region["operation"]
                .as_str()
                .unwrap()
                .ends_with(" Float64Math(Abs)")
        })
        .expect("emitted absolute-value arithmetic without Generic or NativeLeaf");
    assert_eq!(guard["block"], math["block"]);
    assert_eq!(guard["bytePc"], math["bytePc"]);
    for region in [guard, math] {
        assert!(region["startOffset"].as_u64().unwrap() < region["endOffset"].as_u64().unwrap());
        assert!(region["endOffset"].as_u64().unwrap() <= bundle.manifest().code_bytes());
    }
    assert!(guard["endOffset"].as_u64().unwrap() <= math["startOffset"].as_u64().unwrap());
    let relocations: serde_json::Value = serde_json::from_slice(
        bundle
            .file(otter_runtime::JitArtifactFileName::Relocations)
            .expect("actual code relocations")
            .contents(),
    )
    .expect("valid relocations");
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|relocation| {
                relocation["target"]["name"] != "math_abs_leaf"
                    && (relocation["target"]["kind"] != "runtimeStub"
                        || relocation["endOffset"].as_u64().unwrap()
                            <= guard["startOffset"].as_u64().unwrap()
                        || relocation["startOffset"].as_u64().unwrap()
                            >= math["endOffset"].as_u64().unwrap())
            })
    );
}

#[test]
fn numeric_method_candidate_splices_into_optimizing_backend() {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("production-tiered runtime with events");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function apply(value) {
  return this.base + value;
}

const receiver = { base: 4, apply };

function engineKernel(limit) {
  let total = 0;
  for (let i = 0; i < limit; i = i + 1) {
    total = total + receiver.apply(i);
  }
  return total;
}

engineKernel(4000);
"#,
            ),
            "jit-debug-numeric-method-argument.js",
        )
        .expect("monomorphic method loop");
    let report = result
        .jit_debug_report()
        .expect("enabled run owns a report");

    assert_eq!(result.completion_string(), "8014000");
    let engine_kernel = report
        .events()
        .iter()
        .find_map(|event| match event {
            JitDebugEvent::CompilePrepared {
                function_id,
                function_name,
                tier: JitDebugTier::Optimizing,
                target: JitDebugTarget::Osr { .. },
                ..
            } if function_name == "engineKernel" => Some(*function_id),
            _ => None,
        })
        .expect("engineKernel optimizing OSR preparation");
    assert!(
        report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::CompileFinished {
                function_id,
                tier: JitDebugTier::Optimizing,
                target: JitDebugTarget::Osr { .. },
                outcome: JitDebugCompileOutcome::Compiled { .. },
                ..
            } if *function_id == engine_kernel
        )),
        "optimizer must compile the method-inline body: {:?}",
        report.events()
    );
    assert!(
        !report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail {
                function_id,
                tier: JitDebugTier::Optimizing,
                ..
            } if *function_id == engine_kernel
        )),
        "backend selection must not be reported as a runtime side exit: {:?}",
        report.events()
    );
}

#[test]
fn optimized_method_deopt_preserves_this_overflow_invalidation_and_abrupt_completion() {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("production-tiered runtime with events");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function apply(value) {
  return this.base + value;
}

const receiver = { base: 4, apply };

function kernel(start, limit) {
  let total = 0;
  for (let i = 0; i < limit; i = i + 1) {
    total = total + receiver.apply(start + i);
  }
  return total;
}

const hot = kernel(0, 4000);

// The method guard remains valid, but the inlined add overflows int32. The
// side exit must reconstruct both frames at the exact callee PC.
receiver.base = 2147483647;
const overflow = kernel(1, 2);

// Replacing the method without changing the receiver shape must reject the
// cached identity before entry and execute the replacement exactly once.
receiver.base = 10;
receiver.apply = function replacement(value) {
  return this.base * 2 + value;
};
const replaced = kernel(0, 3);

// A second replacement exercises guard-miss deopt through catch/finally. No
// already-started call may be replayed after its throw.
receiver.apply = function abrupt(value) {
  if (value === 1) throw new Error("boom");
  return value;
};
let abrupt = 0;
try {
  abrupt = kernel(0, 4);
} catch (error) {
  abrupt = 100;
} finally {
  abrupt = abrupt + 7;
}

[hot, overflow, replaced, abrupt].join("|");
"#,
            ),
            "jit-debug-optimized-method-deopt.js",
        )
        .expect("method deopt and invalidation fixture");
    let report = result
        .jit_debug_report()
        .expect("enabled run owns a report");

    assert_eq!(result.completion_string(), "8014000|4294967297|63|107");
    assert!(
        report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::CompileFinished {
                tier: JitDebugTier::Optimizing,
                outcome: JitDebugCompileOutcome::Compiled { .. },
                ..
            }
        )),
        "fixture must enter the optimizing tier before its side exits: {:?}",
        report.events()
    );
}

#[test]
fn abrupt_throw_leaves_report_available_for_explicit_drain() {
    let mut runtime = runtime_with_events();
    let source = format!("{HOT_LOOP}\nthrow new Error('after hot loop');");

    runtime
        .run_script(SourceInput::from_javascript(source), "jit-debug-abrupt.js")
        .expect_err("fixture must throw after tier-up");

    let report = runtime
        .take_jit_debug_report()
        .expect("abrupt completion retains the enabled batch");
    assert!(!report.is_empty());
    assert_ordered_template_compile(report.events());
}

#[test]
fn owned_report_survives_full_gc_later_allocation_and_nested_jit() {
    let mut runtime = runtime_with_events();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(NESTED_JIT),
            "jit-debug-nested.js",
        )
        .expect("nested JIT fixture");
    let report = result
        .jit_debug_report()
        .expect("nested run owns a report")
        .clone();
    let stable_copy = report.clone();
    let compiled_functions = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::CompilePrepared { function_id, .. } => Some(*function_id),
            _ => None,
        })
        .collect::<BTreeSet<_>>();

    assert!(
        compiled_functions.len() >= 2,
        "fixture must compile nested and outer loop functions: {compiled_functions:?}"
    );
    runtime.force_gc().expect("full GC");
    let allocation = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
const allocated = [];
for (let i = 0; i < 256; i++) {
  allocated.push({ index: i, label: "entry-" + i });
}
allocated.length;
"#,
            ),
            "jit-debug-post-gc-allocation.js",
        )
        .expect("allocation after full GC");

    assert_eq!(allocation.completion_string(), "256");
    assert_eq!(report, stable_copy);
    assert_ordered_template_compile(report.events());
}

#[tokio::test(flavor = "current_thread")]
async fn async_otter_success_carries_owned_report() {
    let otter = Otter::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("async Otter");
    let result = otter.run_script(HOT_LOOP).await.expect("async hot loop");
    let report = result
        .jit_debug_report()
        .expect("async result carries the owned report");

    assert_eq!(result.completion_string(), "1999000");
    assert_ordered_template_compile(report.events());
}

#[tokio::test(flavor = "current_thread")]
async fn async_otter_report_includes_jit_from_event_loop_callbacks() {
    let otter = Otter::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("async Otter");
    let result = otter
        .run_script(
            r#"
setTimeout(() => {
  function lateHot(limit) {
    let total = 0;
    for (let i = 0; i < limit; i++) total += i;
    return total;
  }
  globalThis.lateResult = lateHot(2000);
}, 0);
"#,
        )
        .await
        .expect("timer callback");
    let report = result
        .jit_debug_report()
        .expect("event-loop run carries the owned report");

    assert!(report.events().iter().any(|event| matches!(
        event,
        JitDebugEvent::CompilePrepared { function_name, .. }
            if function_name == "lateHot"
    )));
    assert_ordered_template_compile(report.events());
}

#[tokio::test(flavor = "current_thread")]
async fn async_otter_abrupt_failure_carries_partial_report() {
    let otter = Otter::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("async Otter");
    let source = format!("{HOT_LOOP}\nthrow new Error('after hot loop');");
    let attempt = otter
        .run_script_source_with_diagnostics(
            SourceInput::from_javascript(source),
            "jit-debug-async-abrupt.js",
        )
        .await;

    assert!(attempt.result().is_err(), "fixture must fail");
    let report = attempt
        .jit_debug_report()
        .expect("failure carries its partial report");
    assert!(!report.is_empty());
    assert_ordered_template_compile(report.events());
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_handle_commands_keep_jit_event_batches_isolated() {
    let otter = Otter::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("async Otter");
    let first_otter = otter.clone();
    let second_otter = otter.clone();
    let first = async move {
        first_otter
            .run_script_source_with_diagnostics(
                SourceInput::from_javascript(
                    r#"
setTimeout(() => {
  function firstLate(limit) {
    let total = 0;
    for (let i = 0; i < limit; i++) total += i;
    return total;
  }
  globalThis.firstResult = firstLate(2000);
}, 20);
"#,
                ),
                "jit-debug-concurrent-first.js",
            )
            .await
    };
    let second = async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        second_otter
            .run_script_source_with_diagnostics(
                SourceInput::from_javascript(
                    r#"
function secondHot(limit) {
  let total = 0;
  for (let i = 0; i < limit; i++) total += i;
  return total;
}
secondHot(2000);
"#,
                ),
                "jit-debug-concurrent-second.js",
            )
            .await
    };
    let (first_attempt, second_attempt) = tokio::join!(first, second);
    let first_report = first_attempt
        .jit_debug_report()
        .expect("first command report");
    let second_report = second_attempt
        .jit_debug_report()
        .expect("second command report");
    let contains_function = |report: &otter_runtime::JitDebugReport, expected: &str| {
        report.events().iter().any(|event| {
            matches!(
                event,
                JitDebugEvent::CompilePrepared { function_name, .. }
                    if function_name == expected
            )
        })
    };

    assert!(first_attempt.result().is_ok());
    assert!(second_attempt.result().is_ok());
    assert!(contains_function(first_report, "firstLate"));
    assert!(!contains_function(first_report, "secondHot"));
    assert!(contains_function(second_report, "secondHot"));
    assert!(!contains_function(second_report, "firstLate"));
}
