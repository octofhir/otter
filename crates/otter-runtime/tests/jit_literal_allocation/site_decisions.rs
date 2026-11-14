//! Placement decisions while an exact generated callee owns literal allocation.
//!
//! # Contents
//! - Object, tagged-array and boilerplate-array tracking generations.
//! - Final young placement without exits, and pre-effect old-placement exits.
//! - Moving collection inside the callee with exact call-cell and source proofs.
//!
//! # Invariants
//! Normal calls earn admission with the runtime's default compiler scheduling.
//! Only the allocation policy decision is controlled, after artifacts prove a
//! tracking generation. GC and native admission remain enabled throughout.
//! Observations retain scalar offsets, never unrooted values across GC.
//! A preceding visible counter detects replay of work before the literal.
//!
//! # See also
//! - `otter_gc::pretenuring` owns final decisions and counter digestion.
//! - `jit_canonical_homes_gc` covers natural decisions inside nested OSR.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use otter_gc::pretenuring::PretenureDecision;
use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeValue, SourceInput,
    ValueWindow,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{ExitAction, ExitReason, NativeFrameKind},
};

#[derive(Default)]
struct Observation {
    decision: Option<PretenureDecision>,
    recording: bool,
    motions: Vec<(u32, u32)>,
    steps: Vec<(u32, otter_bytecode::Op)>,
}

struct Tracer(Arc<Mutex<Observation>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut state = self.0.lock().unwrap();
        if state.recording {
            state.steps.push((event.function_id, event.op));
        }
    }
}

fn offset(value: RuntimeValue) -> u32 {
    value
        .as_object()
        .map(|object| object.offset())
        .or_else(|| value.as_array().map(|array| array.offset()))
        .unwrap()
}

fn fixture(state: Arc<Mutex<Observation>>) -> Runtime {
    let mut runtime = Runtime::builder()
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(RuntimeExtensionInstaller::new({
            let state = state.clone();
            move |realm| {
                realm.install_native_global_call(
                    "siteOffset",
                    1,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        |_ctx: &mut RuntimeNativeCtx<'_>,
                         args: ValueWindow<'_>,
                         _: ValueWindow<'_>| {
                            Ok(RuntimeValue::number_i32(offset(args[0]) as i32))
                        },
                    )),
                )?;
                realm.install_native_global_call(
                    "siteGC",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        |ctx: &mut RuntimeNativeCtx<'_>, _: ValueWindow<'_>, _: ValueWindow<'_>| {
                            ctx.interp_mut().force_gc().unwrap();
                            Ok(RuntimeValue::undefined())
                        },
                    )),
                )?;
                let observation = state.clone();
                realm.install_native_global_call(
                    "siteDecide",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              _: ValueWindow<'_>,
                              _: ValueWindow<'_>| {
                            let state = observation.lock().unwrap();
                            let sites = ctx.interp_mut().gc_heap_mut().allocation_sites_mut();
                            let mut changed = 0;
                            for site in 1..=u16::MAX {
                                if !sites.contains(site) {
                                    break;
                                }
                                if sites.decision(site).tracks() {
                                    sites.set_decision_for_test(site, state.decision.unwrap());
                                    changed += 1;
                                }
                            }
                            assert!(changed > 0, "the admitted fixture retains tracking sites");
                            Ok(RuntimeValue::undefined())
                        },
                    )),
                )?;
                let observation = state.clone();
                realm.install_native_global_call(
                    "siteObserve",
                    1,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              args: ValueWindow<'_>,
                              _: ValueWindow<'_>| {
                            let recording = observation.lock().unwrap().recording;
                            if recording {
                                let motion = ctx.scope(|mut scope| {
                                    let value = scope.argument(args, 0);
                                    let observer = scope.global("siteOffset").unwrap();
                                    let collector = scope.global("siteGC").unwrap();
                                    let this = scope.undefined();
                                    let before = scope.call(observer, this, &[value])?;
                                    let before = scope.number_value(before)? as i32 as u32;
                                    scope.call(collector, this, &[])?;
                                    let after = scope.call(observer, this, &[value])?;
                                    Ok::<_, otter_runtime::RuntimeNativeError>((
                                        before,
                                        scope.number_value(after)? as i32 as u32,
                                    ))
                                })?;
                                observation.lock().unwrap().motions.push(motion);
                            }
                            Ok(RuntimeValue::undefined())
                        },
                    )),
                )
            }
        }))
        .build()
        .unwrap();
    runtime.set_tracer(Some(Box::new(Tracer(state))));
    runtime
}

fn current(
    runtime: &Runtime,
    artifacts: &BTreeMap<u64, JitArtifactBundle>,
    name: &str,
) -> Option<JitCodeGenerationSnapshot> {
    runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .find(|generation| {
            generation.current_entry
                && generation.linked
                && generation.tier == NativeFrameKind::Optimizing
                && artifacts
                    .get(&generation.code_object_id)
                    .is_some_and(|bundle| bundle.manifest().function_name() == name)
        })
}

fn json(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> serde_json::Value {
    serde_json::from_slice(bundle.file(file).unwrap().contents()).unwrap()
}

#[test]
fn generated_literal_placement_decisions_preserve_young_entries_and_exit_before_tenuring() {
    for (case, (literal, check)) in [
        ("{child: input}", "value.child === input"),
        (
            "[input, , input]",
            "value[0] === input && value[2] === input && !(1 in value)",
        ),
        (
            "[1.5, , 7]",
            "value[0] === 1.5 && value[2] === 7 && !(1 in value)",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        for decision in [PretenureDecision::DontTenure, PretenureDecision::Tenure] {
            let state = Arc::new(Mutex::new(Observation::default()));
            let mut runtime = fixture(state.clone());
            let setup = format!(
                r#"
function siteBirth(input) {{
  if (arguments === input) throw "arguments identity";
  input.attempts = input.attempts + 1;
  const value = {literal};
  siteObserve(value);
  if (!({check})) throw "literal corruption";
  return value;
}}
function siteBridge(fn, input) {{ const value = fn(input); return value; }}
const input = {{marker: 7, attempts: 0}};
"#
            );
            runtime
                .run_script(SourceInput::from_javascript(setup), "site-definitions.js")
                .unwrap();
            let mut artifacts = BTreeMap::new();
            for batch in 0..128 {
                let result = runtime
                    .run_script(
                        SourceInput::from_javascript("siteBridge(siteBirth, input);\n".repeat(32)),
                        &format!("site-admission-{batch}.js"),
                    )
                    .unwrap();
                for bundle in result.jit_artifacts().unwrap().bundles() {
                    artifacts.insert(bundle.manifest().code_object_id(), bundle.clone());
                }
                if ["siteBirth", "siteBridge"]
                    .iter()
                    .all(|name| current(&runtime, &artifacts, name).is_some())
                {
                    break;
                }
            }
            let callee =
                current(&runtime, &artifacts, "siteBirth").expect("ordinary callee admission");
            let caller =
                current(&runtime, &artifacts, "siteBridge").expect("ordinary caller admission");
            let deopt = json(
                &artifacts[&callee.code_object_id],
                JitArtifactFileName::Deopt,
            );
            let placement_exits: Vec<_> = deopt["exits"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|exit| exit["reason"] == "allocationSiteDecided")
                .collect();
            assert_eq!(
                placement_exits.len(),
                1,
                "the exact generation must have compiled a tracking site: {literal}: {deopt}"
            );
            let allocation_pc = placement_exits[0]["resumePcs"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .as_u64()
                .unwrap() as u32;
            let relocations = json(
                &artifacts[&caller.code_object_id],
                JitArtifactFileName::Relocations,
            );
            assert!(
                relocations["relocations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["target"]["kind"] == "functionEntryCell"
                        && row["target"]["functionId"] == callee.function_id)
            );
            state.lock().unwrap().decision = Some(decision);
            runtime
                .run_script(
                    SourceInput::from_javascript("siteDecide(); input.attempts = 0;"),
                    "site-decision.js",
                )
                .unwrap();
            state.lock().unwrap().recording = true;
            let before = runtime.execution_stats();
            let result = runtime
                .run_script(
                    SourceInput::from_javascript(
                        "const kept = siteBridge(siteBirth, input); input.marker === 7 && input.attempts === 1;",
                    ),
                    "site-probe.js",
                )
                .unwrap();
            state.lock().unwrap().recording = false;
            if let Some(root) = std::env::var_os("OTTER_LITERAL_PROOF_ARTIFACTS") {
                let directory =
                    std::path::PathBuf::from(root).join(format!("site-{case}-{decision:?}"));
                std::fs::create_dir_all(&directory).unwrap();
                std::fs::write(
                    directory.join("events.json"),
                    serde_json::to_vec_pretty(result.jit_debug_report().unwrap()).unwrap(),
                )
                .unwrap();
                for generation in [&caller, &callee] {
                    let bundle = &artifacts[&generation.code_object_id];
                    let directory = directory.join(bundle.manifest().function_name());
                    std::fs::create_dir(&directory).unwrap();
                    std::fs::write(
                        directory.join("manifest.json"),
                        serde_json::to_vec_pretty(bundle.manifest()).unwrap(),
                    )
                    .unwrap();
                    for file in bundle.files() {
                        std::fs::write(directory.join(file.name().as_str()), file.contents())
                            .unwrap();
                    }
                }
            }
            assert_eq!(result.completion_string(), "true");
            let after = runtime.execution_stats();
            assert!(after.gc_minor_cycles > before.gc_minor_cycles);
            let state = state.lock().unwrap();
            assert_eq!(
                state.motions.len(),
                1,
                "the call executes once, with no replay"
            );
            assert_eq!(
                state.motions[0].0 != state.motions[0].1,
                decision == PretenureDecision::DontTenure
            );
            assert!(
                !state
                    .steps
                    .iter()
                    .any(|(fid, _)| *fid == caller.function_id),
                "caller stays native"
            );
            let report = result.jit_debug_report().unwrap();
            assert!(!report.truncated());
            assert_eq!(report.dropped_events(), 0);
            assert!(
                !report
                    .events()
                    .iter()
                    .any(|event| matches!(event, JitDebugEvent::CompilePrepared { .. })),
                "the isolated probe uses the admitted generations"
            );
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
            if decision == PretenureDecision::DontTenure {
                assert!(
                    exits.is_empty(),
                    "young placement must keep native ownership: {exits:?}"
                );
                assert!(
                    !state
                        .steps
                        .iter()
                        .any(|(fid, _)| *fid == callee.function_id)
                );
                for generation in [&caller, &callee] {
                    let now = runtime
                        .jit_code_generation_snapshot()
                        .into_iter()
                        .find(|now| now.code_object_id == generation.code_object_id)
                        .unwrap();
                    assert!(now.current_entry && now.linked);
                    assert_eq!(now.active_count, 0);
                    assert_eq!(now.generated_deopts, generation.generated_deopts);
                }
            } else {
                assert_eq!(
                    exits.len(),
                    1,
                    "one pre-effect placement transition: {exits:?}"
                );
                assert!(
                    matches!(exits[0], JitDebugEvent::EnteredGenerationDeopt {
                    callee_function_id, callee_code_object_id, callee_resume_pc,
                    exit_reason: ExitReason::AllocationSiteDecided, exit_action: ExitAction::Recompile, ..
                } if *callee_function_id == callee.function_id && *callee_code_object_id == callee.code_object_id && *callee_resume_pc == allocation_pc),
                    "{exits:?}"
                );
            }
            drop(state);
            runtime.force_gc().unwrap();
            let check = format!("const value = kept; ({check}) && input.marker === 7;");
            assert_eq!(
                runtime
                    .run_script(SourceInput::from_javascript(check), "site-reuse.js")
                    .unwrap()
                    .completion_string(),
                "true"
            );
        }
    }
}
