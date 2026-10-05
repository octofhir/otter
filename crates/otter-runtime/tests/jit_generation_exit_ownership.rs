//! Exact-generation policy for exits after reentrant native replacement.
//!
//! # Contents
//! - An active old Graph body resumes after a callback installs its replacement.
//! - A stale guard reports its history without retiring the new generation.
//! - Owned checkpoints join source feedback, retained mappings, and exact code ids.
//!
//! # Invariants
//! - Production compilation and source-work admission run unchanged.
//! - The final old guard sees the exact receiver already used by replacement
//!   calls; it introduces no new source feedback or representation widening.
//! - Capture retains existing source Arcs and owned snapshots, never GC values.
//! - Native execution is proved by exact exits and a complete recovery trace.
//!
//! # See also
//! - `otter_vm::interp::jit_calls::generated` for exact entered exits.
//! - `otter_vm::interp::jit_call` for the shared source-site policy owner.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::sync::{Arc, Mutex};

use otter_bytecode::Op;
use otter_jit::OtterJitCompiler;
use otter_runtime::{
    JitArtifactFileName, JitDebugCompileOutcome, JitDebugEvent, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    CodeBlock, JitCodeGenerationSnapshot, JitCompileError, JitCompileRequest, JitCompileStatus,
    JitCompilerHook, JitRuntimeStubBinding,
    inspect::IcSiteState,
    native_abi::{CodeLifetimeState, ExitAction, ExitReason, NativeFrameKind},
};

const READER: &str = "generationReader";
const SETUP: &str = r#"
let generationReceiver = { x: 1 };
const generationReplacementReceiver = { x: 2, extra: 0 };
let generationArmed = false;
let generationTriggers = 0;

function generationReader(callback) {
  if (arguments.length !== 1) throw "bad reader arguments";
  callback();
  return generationReceiver.x;
}

function generationTrigger() {
  if (arguments.length !== 0) throw "bad callback arguments";
  if (!generationArmed) return;
  generationArmed = false;
  generationTriggers++;
  generationReceiver = generationReplacementReceiver;
  generationReader(generationTrigger);
  for (let batch = 0; batch < 128 && !replacementReady(); batch++) {
    for (let i = 0; i < 32; i++) generationReader(generationTrigger);
  }
  if (!replacementReady()) throw "replacement was not admitted";
  generationCheckpoint(1);
}

for (let i = 0; i < 5000; i++) generationReader(generationTrigger);
"#;

const PROBE: &str = r#"
generationArmed = true;
const first = generationReader(generationTrigger);
generationCheckpoint(2);
const recovered = generationReader(generationTrigger);
generationCheckpoint(3);
JSON.stringify([first, recovered, generationTriggers]);
"#;

#[derive(Debug)]
struct Checkpoint {
    phase: u32,
    epoch: u32,
    property: IcSiteState,
    generations: Vec<JitCodeGenerationSnapshot>,
    trace_len: usize,
}

#[derive(Default)]
struct Observation {
    source: Option<Arc<CodeBlock>>,
    property_site: Option<u32>,
    property_pc: u32,
    property_byte_pc: u32,
    old_code_id: u64,
    recording: bool,
    ticks: Vec<(u32, String, u32, Op)>,
    dropped_ticks: usize,
    checkpoints: Vec<Checkpoint>,
}

struct CaptureCompiler {
    inner: OtterJitCompiler,
    observed: Arc<Mutex<Observation>>,
}

impl CaptureCompiler {
    fn capture(&self, request: &JitCompileRequest) {
        if !request
            .artifact_identity
            .as_ref()
            .is_some_and(|identity| identity.function_name == READER)
        {
            return;
        }
        let source = &request.snapshot.code_block;
        let (pc, instruction) = request
            .snapshot
            .instructions
            .iter()
            .enumerate()
            .filter(|(pc, _)| source.op_at(*pc) == Some(Op::LoadProperty))
            .next_back()
            .expect("reader ends by loading the receiver's x property");
        let mut observed = self.observed.lock().unwrap();
        if let Some(previous) = &observed.source {
            assert!(Arc::ptr_eq(previous, source));
        }
        observed.source = Some(source.clone());
        observed.property_pc = pc as u32;
        observed.property_byte_pc = instruction.byte_pc;
        observed.property_site = instruction.property_ic_site(source).map(|site| site as u32);
    }
}

impl JitCompilerHook for CaptureCompiler {
    fn optimizing_tier_enabled(&self) -> bool {
        self.inner.optimizing_tier_enabled()
    }
    fn runtime_stub_bindings(&self) -> Vec<JitRuntimeStubBinding> {
        self.inner.runtime_stub_bindings()
    }
    fn compile_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        self.capture(&request);
        self.inner.compile_function(request)
    }
    fn compile_optimized_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        self.capture(&request);
        self.inner.compile_optimized_function(request)
    }
}

struct Trace(Arc<Mutex<Observation>>);

impl StepTracer for Trace {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut observed = self.0.lock().unwrap();
        if !observed.recording {
            return;
        }
        if observed.ticks.len() == 65_536 {
            observed.dropped_ticks += 1;
            return;
        }
        observed.ticks.push((
            event.function_id,
            event.function_name.to_owned(),
            event.byte_pc,
            event.op,
        ));
    }
}

fn current_graph(generations: &[JitCodeGenerationSnapshot]) -> &JitCodeGenerationSnapshot {
    let current: Vec<_> = generations
        .iter()
        .filter(|generation| {
            generation.tier == NativeFrameKind::Optimizing
                && generation.lifecycle == CodeLifetimeState::Installed
                && generation.linked
        })
        .collect();
    assert_eq!(
        current.len(),
        1,
        "one current reader Graph: {generations:?}"
    );
    current[0]
}

#[test]
fn old_graph_guard_exit_preserves_the_reentrant_current_replacement() {
    let observed = Arc::new(Mutex::new(Observation::default()));
    let compiler: Arc<dyn JitCompilerHook> = Arc::new(CaptureCompiler {
        inner: OtterJitCompiler::production_tiered(),
        observed: observed.clone(),
    });
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(RuntimeExtensionInstaller::new({
            let observed = observed.clone();
            move |realm| {
                let compiler = compiler.clone();
                realm.install_native_global_call(
                    "installGenerationCompiler",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              _args: &[RuntimeValue],
                              _state: &[RuntimeValue]| {
                            ctx.interp_mut().set_jit_compiler(Some(compiler.clone()));
                            Ok(RuntimeValue::undefined())
                        },
                    )),
                )?;
                let ready = observed.clone();
                realm.install_native_global_call(
                    "replacementReady",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              _args: &[RuntimeValue],
                              _state: &[RuntimeValue]| {
                            let observed = ready.lock().unwrap();
                            let fid = observed.source.as_ref().unwrap().id;
                            let ready = ctx.interp_mut().jit_code_generation_snapshot().iter().any(
                                |generation| {
                                    generation.function_id == fid
                                        && generation.code_object_id != observed.old_code_id
                                        && generation.tier == NativeFrameKind::Optimizing
                                        && generation.lifecycle == CodeLifetimeState::Installed
                                        && generation.linked
                                },
                            );
                            Ok(RuntimeValue::boolean(ready))
                        },
                    )),
                )?;
                let checkpoint = observed.clone();
                realm.install_native_global_call(
                    "generationCheckpoint",
                    1,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              args: &[RuntimeValue],
                              _state: &[RuntimeValue]| {
                            let mut observed = checkpoint.lock().unwrap();
                            let source = observed.source.as_ref().unwrap();
                            let fid = source.id;
                            let epoch = source.feedback_epoch();
                            let property_site = observed.property_site.unwrap();
                            let property = ctx
                                .interp_mut()
                                .ic_snapshot()
                                .into_iter()
                                .find(|site| site.site_index == property_site)
                                .unwrap()
                                .state;
                            let generations = ctx
                                .interp_mut()
                                .jit_code_generation_snapshot()
                                .into_iter()
                                .filter(|generation| generation.function_id == fid)
                                .collect();
                            let trace_len = observed.ticks.len();
                            observed.checkpoints.push(Checkpoint {
                                phase: args[0].as_f64().unwrap() as u32,
                                epoch,
                                property,
                                generations,
                                trace_len,
                            });
                            Ok(RuntimeValue::undefined())
                        },
                    )),
                )
            }
        }))
        .build()
        .unwrap();
    runtime.set_tracer(Some(Box::new(Trace(observed.clone()))));
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(format!("installGenerationCompiler();\n{SETUP}")),
            "jit-generation-exit-setup.js",
        )
        .unwrap();
    let fid = observed.lock().unwrap().source.as_ref().unwrap().id;
    let reader_generations: Vec<_> = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|generation| generation.function_id == fid)
        .collect();
    let old = current_graph(&reader_generations).clone();
    let artifacts = setup.jit_artifacts().unwrap();
    assert!(!artifacts.truncated());
    let old_bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| bundle.manifest().code_object_id() == old.code_object_id)
        .unwrap();
    let map: serde_json::Value = serde_json::from_slice(
        old_bundle
            .file(JitArtifactFileName::CodeMap)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let mut observation = observed.lock().unwrap();
    assert!(
        map["regions"].as_array().unwrap().iter().any(|region| {
            region["bytePc"].as_u64() == Some(u64::from(observation.property_byte_pc))
                && region["operation"]
                    .as_str()
                    .is_some_and(|op| op.contains("CheckShapes"))
                && region["endOffset"].as_u64().unwrap() > region["startOffset"].as_u64().unwrap()
        }),
        "the exact old own Graph emits the stale receiver guard"
    );
    observation.old_code_id = old.code_object_id;
    observation.recording = true;
    drop(observation);
    drop(setup);
    let probe = runtime
        .run_script(
            SourceInput::from_javascript(PROBE),
            "jit-generation-exit-probe.js",
        )
        .unwrap();
    assert_eq!(probe.completion_string(), "[2,2,1]");
    let report = probe.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let observation = observed.lock().unwrap();
    assert_eq!(
        observation.dropped_ticks, 0,
        "complete isolated recovery trace"
    );
    assert_eq!(observation.checkpoints.len(), 3);
    let [before_old_exit, after_old_exit, after_recovery] = observation.checkpoints.as_slice()
    else {
        unreachable!()
    };
    assert_eq!(
        [
            before_old_exit.phase,
            after_old_exit.phase,
            after_recovery.phase
        ],
        [1, 2, 3]
    );
    let replacement = current_graph(&before_old_exit.generations);
    assert_ne!(replacement.code_object_id, old.code_object_id);
    assert_eq!(replacement.generated_deopts, 0);
    let retained_old = before_old_exit
        .generations
        .iter()
        .find(|generation| generation.code_object_id == old.code_object_id)
        .unwrap();
    assert_eq!(retained_old.lifecycle, CodeLifetimeState::Invalid);
    assert!(!retained_old.linked);
    // Generated frames pin the native retirement epoch, rather than taking
    // optional Rust-side CodeEntryLease ownership on every call. Invalid
    // executable metadata remains registered until that old extent returns:
    // its one published frame is the only activity it reports.
    assert!(retained_old.dependencies.is_some());
    assert_eq!(retained_old.active_count, 1);
    for checkpoint in [after_old_exit, after_recovery] {
        assert_eq!(
            current_graph(&checkpoint.generations).code_object_id,
            replacement.code_object_id
        );
        assert_eq!(current_graph(&checkpoint.generations).generated_deopts, 0);
        assert_eq!(
            checkpoint.epoch, before_old_exit.epoch,
            "stale guard introduces no new feedback"
        );
        assert_eq!(
            checkpoint.property, before_old_exit.property,
            "exact receiver programs remain unchanged"
        );
    }
    let exits: Vec<_> = report
        .events()
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            JitDebugEvent::EnteredGenerationDeopt {
                callee_function_id,
                callee_code_object_id,
                callee_tier: JitDebugTier::Optimizing,
                callee_resume_pc,
                exit_reason: ExitReason::ShapeGuard,
                exit_action: ExitAction::Recompile,
            } if *callee_function_id == fid
                && *callee_code_object_id == old.code_object_id
                && *callee_resume_pc == observation.property_pc =>
            {
                Some(index)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        exits.len(),
        2,
        "nested and resumed exits belong to the same old Graph"
    );
    let replacement_compile = report
        .events()
        .iter()
        .position(|event| {
            matches!(event,
                JitDebugEvent::CompileFinished { function_id, tier: JitDebugTier::Optimizing,
                    outcome: JitDebugCompileOutcome::Compiled { code_object_id, .. }, .. }
                    if *function_id == fid && *code_object_id == replacement.code_object_id
            )
        })
        .unwrap();
    assert!(exits[0] < replacement_compile && replacement_compile < exits[1]);
    assert!(!report.events().iter().any(|event| matches!(event,
        JitDebugEvent::EnteredGenerationDeopt { callee_code_object_id, .. }
            if *callee_code_object_id == replacement.code_object_id
    )));
    assert!(
        !report.events()[exits[1] + 1..]
            .iter()
            .any(|event| matches!(event,
                JitDebugEvent::CompilePrepared { function_id, .. } if *function_id == fid
            ))
    );
    assert!(
        !observation.ticks[after_old_exit.trace_len..after_recovery.trace_len]
            .iter()
            .any(|(tick_fid, _, _, _)| *tick_fid == fid),
        "recovery enters the unchanged replacement natively"
    );
    assert!(
        observation
            .ticks
            .iter()
            .any(|(_, name, _, op)| name == "<main>" && *op == Op::Call)
    );
    assert!(
        observation
            .ticks
            .iter()
            .any(|(_, name, _, op)| name == "<main>"
                && matches!(op, Op::Return | Op::ReturnValue | Op::ReturnUndefined))
    );
}
