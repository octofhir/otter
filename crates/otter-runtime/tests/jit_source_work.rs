//! Independent execution oracles for source-opcode tier work.
//!
//! # Contents
//! - Complete interpreter step traces, joined to captured immutable CodeBlocks.
//! - Template CFG, fusion, method, recursion, throw, and reentry boundaries.
//! - Once-called OSR loops and uncharged optimizing execution.
//!
//! # Invariants
//! - Expected work comes from actually dispatched oracle instructions, not a
//!   static body length, compiler lowering count, or the counter under test.
//! - Compiler capture retains existing CodeBlock Arcs and delegates production
//!   compilation unchanged; the interpreter control only declines compilation.
//! - Checkpoints retain scalar counts and instruction identity, never Values,
//!   register-window borrows, native frame pointers, or collector roots.
//! - Native execution must be demonstrated separately from successful emission.
//! - Every probe has a fresh, non-looping main; only the named OSR subject loops.
//!
//! # See also
//! - `jit_speculative_property_loads` for isolated generated-entry proofs.
//! - `otter_vm::native_abi::SourceWork` for the one source-function work cell.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use otter_bytecode::Op;
use otter_jit::OtterJitCompiler;
use otter_runtime::{
    JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitSelection, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    CodeBlock, JitCompileError, JitCompileRequest, JitCompileStatus, JitCompilerHook,
    JitRuntimeStubBinding,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};

type Counts = BTreeMap<String, u64>;

#[derive(Clone, Debug)]
struct Tick {
    fid: u32,
    name: String,
    depth: usize,
    byte_pc: u32,
    op: Op,
}

#[derive(Clone, Debug)]
struct Checkpoint {
    phase: u32,
    work: Counts,
    traced: Counts,
    trace_len: usize,
    optimized_osr_entries: u64,
}

#[derive(Clone, Debug)]
struct Compilation {
    fid: u32,
    code_id: u64,
    tier: NativeFrameKind,
    osr_pc: Option<u32>,
    source_work: u64,
    last_phase: Option<u32>,
    trace_len: usize,
    succeeded: bool,
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))] // Template body fusion is ARM-only.
    spliced: Vec<u32>,
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    fused: bool,
    outcome: String,
    /// Bounded source identities and numeric feedback from this exact request.
    input: Vec<(u32, u32, Op, bool)>,
    leaders: Vec<u32>,
}

#[derive(Default)]
struct Observation {
    blocks: BTreeMap<u32, Arc<CodeBlock>>,
    layouts: BTreeMap<u32, BTreeMap<u32, Op>>,
    names: BTreeMap<u32, String>,
    compilations: Vec<Compilation>,
    recording: bool,
    baseline: BTreeMap<u32, u64>,
    ticks: Vec<Tick>,
    trace_counts: Counts,
    checkpoints: Vec<Checkpoint>,
    setup_events: Vec<JitDebugEvent>,
}

impl Observation {
    fn totals(&self) -> Counts {
        let mut totals = Counts::new();
        for (&fid, block) in &self.blocks {
            if let Some(name) = self.names.get(&fid) {
                let before = self.baseline.get(&fid).copied().unwrap_or(0);
                let delta = block.source_work().total().checked_sub(before).unwrap();
                *totals.entry(name.clone()).or_default() += delta;
            }
        }
        totals
    }

    fn traced(&self) -> Counts {
        self.trace_counts.clone()
    }

    fn begin_probe(&mut self) {
        self.baseline = self
            .blocks
            .iter()
            .map(|(&fid, block)| (fid, block.source_work().total()))
            .collect();
        self.ticks.clear();
        self.trace_counts.clear();
        self.checkpoints.clear();
        self.recording = true;
    }
}

struct OracleTrace(Arc<Mutex<Observation>>);

impl StepTracer for OracleTrace {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut observation = self.0.lock().unwrap();
        observation
            .names
            .insert(event.function_id, event.function_name.to_owned());
        if observation.recording {
            *observation
                .trace_counts
                .entry(event.function_name.to_owned())
                .or_default() += 1;
            observation.ticks.push(Tick {
                fid: event.function_id,
                name: event.function_name.to_owned(),
                depth: event.frame_depth,
                byte_pc: event.byte_pc,
                op: event.op,
            });
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Policy {
    Oracle,
    DeclinedInterpreter,
    Template,
    Production,
}

/// This wrapper changes no production compile request or returned code object.
struct CapturingCompiler {
    inner: OtterJitCompiler,
    observations: Arc<Mutex<Observation>>,
    decline: bool,
}

impl CapturingCompiler {
    fn begin(&self, request: &JitCompileRequest, tier: NativeFrameKind) -> usize {
        let mut observation = self.observations.lock().unwrap();
        // Candidate snapshots can be nested. Capture identity only; admission
        // or successful splicing is proved from the returned code object.
        let mut snapshots = vec![&request.snapshot];
        let mut seen = BTreeSet::new();
        while let Some(snapshot) = snapshots.pop() {
            let fid = snapshot.code_block.id;
            if !seen.insert(fid) {
                continue;
            }
            if let Some(previous) = observation.blocks.insert(fid, snapshot.code_block.clone()) {
                assert!(Arc::ptr_eq(&previous, &snapshot.code_block));
            }
            observation.layouts.insert(
                fid,
                snapshot
                    .instructions
                    .iter()
                    .enumerate()
                    .map(|(index, instruction)| {
                        (
                            instruction.byte_pc,
                            snapshot.code_block.op_at(index).unwrap(),
                        )
                    })
                    .collect(),
            );
            snapshots.extend(
                snapshot
                    .inline_callees
                    .values()
                    .map(|callee| callee.body.as_ref()),
            );
            snapshots.extend(
                snapshot
                    .inline_methods
                    .values()
                    .map(|method| method.body.as_ref()),
            );
            snapshots.extend(
                snapshot
                    .inline_poly_methods
                    .values()
                    .flatten()
                    .map(|method| method.body.as_ref()),
            );
        }
        if let Some(identity) = &request.artifact_identity {
            observation.names.insert(
                request.snapshot.code_block.id,
                identity.function_name.clone(),
            );
        }
        let index = observation.compilations.len();
        let last_phase = observation
            .checkpoints
            .last()
            .map(|checkpoint| checkpoint.phase);
        let trace_len = observation.ticks.len();
        observation.compilations.push(Compilation {
            fid: request.snapshot.code_block.id,
            code_id: request.code_object_id,
            tier,
            osr_pc: request.osr_pc,
            source_work: request.snapshot.code_block.source_work().total(),
            last_phase,
            trace_len,
            succeeded: false,
            spliced: Vec::new(),
            fused: false,
            outcome: "pending hook".into(),
            input: request
                .snapshot
                .instructions
                .iter()
                .enumerate()
                .take(32)
                .map(|(pc, instruction)| {
                    (
                        pc as u32,
                        instruction.byte_pc,
                        request.snapshot.code_block.op_at(pc).unwrap(),
                        request.snapshot.feedback_at(pc as u32).is_numeric_only(),
                    )
                })
                .collect(),
            leaders: request
                .snapshot
                .code_block
                .block_starts()
                .iter()
                .copied()
                .take(32)
                .collect(),
        });
        index
    }

    fn finish(&self, index: usize, result: &Result<JitCompileStatus, JitCompileError>) {
        self.observations.lock().unwrap().compilations[index].outcome = match result {
            Ok(JitCompileStatus::Compiled { artifact, .. }) => {
                format!("compiled; artifact={}", artifact.is_some())
            }
            Ok(JitCompileStatus::Unavailable) => "unavailable".into(),
            Ok(JitCompileStatus::Unsupported { reason }) => format!("unsupported: {reason}"),
            Err(error) => format!("error: {error}"),
        };
        let Ok(JitCompileStatus::Compiled { code, artifact, .. }) = result else {
            return;
        };
        let fused = artifact.as_ref().is_some_and(|bundle| {
            bundle
                .file(JitArtifactFileName::CodeMap)
                .is_some_and(|file| {
                    let map: serde_json::Value = serde_json::from_slice(file.contents()).unwrap();
                    map["regions"].as_array().unwrap().iter().any(|region| {
                        region["operation"]
                            .as_str()
                            .is_some_and(|operation| operation.contains("FusedNumericChain"))
                            && region["endOffset"].as_u64().unwrap()
                                > region["startOffset"].as_u64().unwrap()
                    })
                })
        });
        let mut observation = self.observations.lock().unwrap();
        let compilation = &mut observation.compilations[index];
        compilation.succeeded = true;
        compilation.spliced = code.spliced_functions().to_vec();
        compilation.fused = fused;
    }
}

impl JitCompilerHook for CapturingCompiler {
    fn optimizing_tier_enabled(&self) -> bool {
        !self.decline && self.inner.optimizing_tier_enabled()
    }

    fn runtime_stub_bindings(&self) -> Vec<JitRuntimeStubBinding> {
        self.inner.runtime_stub_bindings()
    }

    fn compile_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        let index = self.begin(&request, NativeFrameKind::Baseline);
        let result = if self.decline {
            Ok(JitCompileStatus::Unavailable)
        } else {
            self.inner.compile_function(request)
        };
        self.finish(index, &result);
        result
    }

    fn compile_optimized_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        let index = self.begin(&request, NativeFrameKind::Optimizing);
        let result = self.inner.compile_optimized_function(request);
        self.finish(index, &result);
        result
    }
}

struct Harness {
    runtime: Runtime,
    observations: Arc<Mutex<Observation>>,
    policy: Policy,
}

#[derive(Debug)]
struct Probe {
    completion: String,
    work: Counts,
    traced: Counts,
    ticks: Vec<Tick>,
    checkpoints: Vec<Checkpoint>,
    events: Vec<JitDebugEvent>,
}

impl Harness {
    fn new(setup: &str, policy: Policy) -> Self {
        let observations = Arc::new(Mutex::new(Observation::default()));
        let compiler: Arc<dyn JitCompilerHook> = Arc::new(CapturingCompiler {
            inner: if policy == Policy::Production {
                OtterJitCompiler::production_tiered()
            } else {
                OtterJitCompiler::template_only()
            },
            observations: observations.clone(),
            decline: policy == Policy::DeclinedInterpreter,
        });
        let mut runtime = Runtime::builder()
            .jit_selection(match policy {
                Policy::Oracle => JitSelection::InterpreterOnly,
                Policy::Production => JitSelection::ProductionTiered,
                _ => JitSelection::Template,
            })
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(RuntimeExtensionInstaller::new({
                let observations = observations.clone();
                move |realm| {
                    let compiler = compiler.clone();
                    realm.install_native_global_call(
                        "installSourceCompiler",
                        0,
                        RuntimeNativeCall::Dynamic(Arc::new(
                            move |ctx: &mut RuntimeNativeCtx<'_>,
                                  _args: &[RuntimeValue],
                                  _state: &[RuntimeValue]| {
                                if policy != Policy::Oracle {
                                    ctx.interp_mut().set_jit_compiler(Some(compiler.clone()));
                                }
                                Ok(RuntimeValue::undefined())
                            },
                        )),
                    )?;
                    let observations = observations.clone();
                    realm.install_native_global_call(
                        "sourceCheckpoint",
                        1,
                        RuntimeNativeCall::Dynamic(Arc::new(
                            move |ctx: &mut RuntimeNativeCtx<'_>,
                                  args: &[RuntimeValue],
                                  _state: &[RuntimeValue]| {
                                let phase = args[0].as_f64().unwrap() as u32;
                                let mut observation = observations.lock().unwrap();
                                if observation.recording {
                                    let work = observation.totals();
                                    let traced = observation.traced();
                                    let trace_len = observation.ticks.len();
                                    observation.checkpoints.push(Checkpoint {
                                        phase,
                                        work,
                                        traced,
                                        trace_len,
                                        optimized_osr_entries: ctx
                                            .interp_mut()
                                            .jit_runtime_stats()
                                            .optimized_osr_entries,
                                    });
                                }
                                Ok(RuntimeValue::undefined())
                            },
                        )),
                    )
                }
            }))
            .build()
            .unwrap();
        runtime.set_tracer(Some(Box::new(OracleTrace(observations.clone()))));
        let result = runtime
            .run_script(
                SourceInput::from_javascript(format!("installSourceCompiler();\n{setup}")),
                "source-work-setup.js",
            )
            .unwrap_or_else(|error| panic!("{policy:?} setup: {error:?}"));
        observations.lock().unwrap().setup_events = result
            .jit_debug_report()
            .expect("setup events enabled")
            .events()
            .iter()
            .rev()
            .take(32)
            .cloned()
            .collect();
        Self {
            runtime,
            observations,
            policy,
        }
    }

    /// Enter warmed source owners independently of an inlining warm parent.
    /// This bounded, non-looping script uses ordinary production admission;
    /// probe baselines are taken later and exclude these setup attempts.
    fn admit_with_calls(&mut self, source: &str) {
        assert!(!self.observations.lock().unwrap().recording);
        let result = self
            .runtime
            .run_script(SourceInput::from_javascript(source), "source-work-admit.js")
            .unwrap_or_else(|error| panic!("{:?} independent setup entry: {error:?}", self.policy));
        let mut observation = self.observations.lock().unwrap();
        let events = result.jit_debug_report().expect("setup events enabled");
        observation
            .setup_events
            .extend(events.events().iter().cloned());
        let excess = observation.setup_events.len().saturating_sub(32);
        observation.setup_events.drain(..excess);
    }

    fn block(&self, name: &str) -> Arc<CodeBlock> {
        let observation = self.observations.lock().unwrap();
        let blocks: Vec<_> = observation
            .blocks
            .iter()
            .filter(|(fid, _)| {
                observation
                    .names
                    .get(fid)
                    .is_some_and(|found| found == name)
            })
            .map(|(_, block)| block.clone())
            .collect();
        assert_eq!(blocks.len(), 1, "one captured source owner for {name}");
        blocks[0].clone()
    }

    fn current(&self, name: &str, tier: NativeFrameKind) -> u64 {
        let fid = self.block(name).id;
        let current: Vec<_> = self
            .runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|generation| {
                generation.function_id == fid
                    && generation.tier == tier
                    && generation.lifecycle == CodeLifetimeState::Installed
                    && generation.linked
            })
            .collect();
        if current.len() != 1 {
            let observation = self.observations.lock().unwrap();
            let attempts: Vec<_> = observation
                .compilations
                .iter()
                .rev()
                .filter(|attempt| attempt.fid == fid)
                .take(4)
                .map(|attempt| {
                    format!(
                        "code={} tier={:?} osr={:?} work={} outcome={} input={:?} leaders={:?}",
                        attempt.code_id,
                        attempt.tier,
                        attempt.osr_pc,
                        attempt.source_work,
                        attempt.outcome,
                        attempt.input,
                        attempt.leaders
                    )
                })
                .collect();
            let generations: Vec<_> = self
                .runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .filter(|generation| generation.function_id == fid)
                .collect();
            panic!(
                "{name} needs one actual current {tier:?}; found {}; sourceWork={}; own attempts={attempts:#?}; generations={generations:#?}; bounded setup events={:#?}",
                current.len(),
                observation.blocks[&fid].source_work().total(),
                observation.setup_events
            );
        }
        current[0].code_object_id
    }

    fn probe(&mut self, source: &str) -> Probe {
        self.observations.lock().unwrap().begin_probe();
        let result = self
            .runtime
            .run_script(SourceInput::from_javascript(source), "source-work-probe.js")
            .unwrap_or_else(|error| panic!("{:?} probe: {error:?}", self.policy));
        let completion = result.completion_string().to_owned();
        let report = result.jit_debug_report().expect("probe events enabled");
        assert!(
            !report.truncated(),
            "small oracle probes must retain every event"
        );
        assert_eq!(report.dropped_events(), 0);
        let events = report.events().to_vec();
        let mut observation = self.observations.lock().unwrap();
        observation.recording = false;
        Probe {
            completion,
            work: observation.totals(),
            traced: observation.traced(),
            ticks: observation.ticks.clone(),
            checkpoints: observation.checkpoints.clone(),
            events,
        }
    }
}

fn count(counts: &Counts, name: &str) -> u64 {
    counts.get(name).copied().unwrap_or(0)
}

fn assert_exact(oracle: &Probe, candidate: &Probe, names: &[&str]) {
    assert_eq!(candidate.completion, oracle.completion);
    for name in names {
        assert_eq!(
            count(&candidate.work, name),
            count(&oracle.traced, name),
            "exact source attempts for {name}: {candidate:?}"
        );
    }
    assert_eq!(candidate.checkpoints.len(), oracle.checkpoints.len());
    for (actual, expected) in candidate.checkpoints.iter().zip(&oracle.checkpoints) {
        assert_eq!(actual.phase, expected.phase);
        assert_eq!(actual.optimized_osr_entries, expected.optimized_osr_entries);
        assert!(actual.trace_len <= candidate.ticks.len());
        assert!(expected.trace_len <= oracle.ticks.len());
        for name in names {
            assert_eq!(
                count(&actual.work, name),
                count(&expected.traced, name),
                "entered Call must be visible before checkpoint {} in {name}",
                actual.phase
            );
        }
    }
}

fn assert_native(candidate: &Probe, names: &[&str]) {
    assert!(
        candidate
            .ticks
            .iter()
            .any(|tick| tick.name == "<main>" && matches!(tick.op, Op::Call | Op::CallWithThis))
    );
    assert!(
        candidate.ticks.iter().any(
            |tick| tick.name == "<main>" && matches!(tick.op, Op::Return | Op::ReturnUndefined)
        )
    );
    for name in names {
        assert!(
            !candidate.ticks.iter().any(|tick| tick.name == *name),
            "{name} must run in native code: {candidate:?}"
        );
    }
}

fn assert_oracle_layout(oracle: &Probe, harness: &Harness, names: &[&str]) {
    for name in names {
        let block = harness.block(name);
        let observation = harness.observations.lock().unwrap();
        let layout = &observation.layouts[&block.id];
        for tick in oracle.ticks.iter().filter(|tick| tick.name == *name) {
            assert_eq!(
                layout.get(&tick.byte_pc),
                Some(&tick.op),
                "oracle byte PC/op belongs to the captured immutable body"
            );
        }
    }
}

const CFG_SETUP: &str = r#"
function cfgWork(short, x) {
    sourceCheckpoint(1);
    if (short) { let result = x + 1; sourceCheckpoint(2); return result; }
    let a = x - 2; let b = a * 3; let c = b + 4;
    sourceCheckpoint(3); return c;
}
for (let warm = 0; warm < 5000; warm++) { cfgWork(true, 7); cfgWork(false, 7); }
"#;

#[test]
fn interpreted_and_template_attempts_follow_cfg_and_count_expanded_addimm_once() {
    let mut oracle = Harness::new(CFG_SETUP, Policy::Oracle);
    for policy in [Policy::DeclinedInterpreter, Policy::Template] {
        let mut candidate = Harness::new(CFG_SETUP, policy);
        for (source, expected) in [("cfgWork(true, 7);", "8"), ("cfgWork(false, 7);", "19")] {
            let generation = (policy == Policy::Template)
                .then(|| candidate.current("cfgWork", NativeFrameKind::Baseline));
            let reference = oracle.probe(source);
            assert_eq!(reference.completion, expected);
            assert!(
                reference
                    .ticks
                    .iter()
                    .any(|tick| tick.name == "cfgWork" && tick.op == Op::AddImm)
            );
            if expected == "8" {
                assert!(
                    !reference
                        .ticks
                        .iter()
                        .any(|tick| tick.name == "cfgWork" && tick.op == Op::Mul)
                );
                assert!(
                    count(&reference.traced, "cfgWork")
                        < candidate.block("cfgWork").code.len() as u64,
                    "the short path leaves source instructions unentered"
                );
            } else {
                assert!(
                    reference
                        .ticks
                        .iter()
                        .any(|tick| tick.name == "cfgWork" && tick.op == Op::Mul)
                );
            }
            let actual = candidate.probe(source);
            assert_oracle_layout(&reference, &candidate, &["cfgWork"]);
            assert_exact(&reference, &actual, &["cfgWork"]);
            if let Some(generation) = generation {
                assert_native(&actual, &["cfgWork"]);
                assert_eq!(
                    candidate.current("cfgWork", NativeFrameKind::Baseline),
                    generation
                );
            } else {
                assert_eq!(
                    count(&actual.traced, "cfgWork"),
                    count(&reference.traced, "cfgWork")
                );
            }
        }
    }
}

const RECURSION_SETUP: &str = r#"
function recursiveWork(n) {
    sourceCheckpoint(100 + n);
    if (n === 0) return 1;
    let value = recursiveWork(n - 1) + n;
    sourceCheckpoint(200 + n);
    return value;
}
function throwingWork(fail, value) {
    sourceCheckpoint(301);
    if (fail) throw value;
    let result = value + 1;
    sourceCheckpoint(302);
    return result;
}
for (let warm = 0; warm < 5000; warm++) {
    recursiveWork(3);
    throwingWork(false, 17);
    try { throwingWork(true, 17); } catch (error) {}
}
"#;

#[test]
fn recursive_activations_flush_independent_prefixes_and_throw_excludes_unentered_suffix() {
    let mut oracle = Harness::new(RECURSION_SETUP, Policy::Oracle);
    let mut candidate = Harness::new(RECURSION_SETUP, Policy::Template);
    for (name, source, expected) in [
        ("recursiveWork", "recursiveWork(3);", "7"),
        (
            "throwingWork",
            "try { throwingWork(true, 17); } catch (error) { error; }",
            "17",
        ),
        ("throwingWork", "throwingWork(false, 17);", "18"),
    ] {
        let generation = candidate.current(name, NativeFrameKind::Baseline);
        let reference = oracle.probe(source);
        assert_eq!(reference.completion, expected);
        let actual = candidate.probe(source);
        assert_exact(&reference, &actual, &[name]);
        assert_oracle_layout(&reference, &candidate, &[name]);
        assert_native(&actual, &[name]);
        assert_eq!(
            candidate.current(name, NativeFrameKind::Baseline),
            generation
        );
        if name == "recursiveWork" {
            let own: Vec<_> = reference
                .ticks
                .iter()
                .filter(|tick| tick.name == name)
                .collect();
            assert_eq!(
                own.iter()
                    .map(|tick| tick.fid)
                    .collect::<BTreeSet<_>>()
                    .len(),
                1
            );
            assert!(
                own.iter().map(|tick| tick.depth).max().unwrap()
                    - own.iter().map(|tick| tick.depth).min().unwrap()
                    >= 3
            );
        } else if expected == "17" {
            assert!(
                reference
                    .ticks
                    .iter()
                    .any(|tick| tick.name == name && tick.op == Op::Throw)
            );
            assert_eq!(
                actual
                    .checkpoints
                    .iter()
                    .map(|point| point.phase)
                    .collect::<Vec<_>>(),
                [301]
            );
        }
    }
}

const FUSION_SETUP: &str = r#"
let coercions = 0, coercionThrows = false;
function coercionWork() {
    coercions++; sourceCheckpoint(401);
    if (coercionThrows) throw 17;
    return 12;
}
const numericBox = {valueOf: coercionWork};
function fusedWork(a, b, c) { return c * (a - b); }
function fusedImmediateWork(a, b) { return b * (a - 3); }
for (let warm = 0; warm < 5000; warm++) {
    numericBox.valueOf(); fusedWork(12, 3, 2); fusedImmediateWork(12, 2);
}
coercions = 0;
"#;

#[test]
#[cfg(target_arch = "aarch64")]
fn numeric_chain_hits_and_coercing_misses_credit_only_entered_source_instructions() {
    let mut oracle = Harness::new(FUSION_SETUP, Policy::Oracle);
    let mut candidate = Harness::new(FUSION_SETUP, Policy::Template);
    candidate.admit_with_calls("fusedWork(12, 3, 2); fusedWork(12, 3, 2);");
    let generation = candidate.current("fusedWork", NativeFrameKind::Baseline);
    assert!(
        candidate
            .observations
            .lock()
            .unwrap()
            .compilations
            .iter()
            .any(|compile| compile.code_id == generation && compile.fused),
        "the target must actually contain a fused numeric chain"
    );
    for (source, expected) in [
        (
            "JSON.stringify([fusedWork(12, 3, 2), coercions]);",
            "[18,0]",
        ),
        (
            "JSON.stringify([fusedWork(numericBox, 3, 2), coercions]);",
            "[18,1]",
        ),
        (
            "coercionThrows = true; try { fusedWork(numericBox, 3, 2); } catch (error) { JSON.stringify([error, coercions]); }",
            "[17,2]",
        ),
    ] {
        let reference = oracle.probe(source);
        assert_eq!(reference.completion, expected);
        let actual = candidate.probe(source);
        assert_exact(&reference, &actual, &["fusedWork", "coercionWork"]);
        assert_oracle_layout(&reference, &candidate, &["fusedWork", "coercionWork"]);
        assert_native(&actual, &["fusedWork", "coercionWork"]);
        assert_eq!(
            candidate.current("fusedWork", NativeFrameKind::Baseline),
            generation
        );
        if expected == "[17,2]" {
            assert!(
                reference
                    .ticks
                    .iter()
                    .any(|tick| tick.name == "fusedWork" && tick.op == Op::Sub)
            );
            assert!(
                !reference
                    .ticks
                    .iter()
                    .any(|tick| tick.name == "fusedWork" && tick.op == Op::Mul)
            );
        }
    }
    assert_eq!(candidate.probe("coercions;").completion, "2");
}

#[test]
fn an_expanded_source_group_inside_a_numeric_chain_counts_once() {
    let mut oracle = Harness::new(FUSION_SETUP, Policy::Oracle);
    let mut candidate = Harness::new(FUSION_SETUP, Policy::Template);
    candidate.admit_with_calls("fusedImmediateWork(12, 2); fusedImmediateWork(12, 2);");
    let generation = candidate.current("fusedImmediateWork", NativeFrameKind::Baseline);
    #[cfg(target_arch = "aarch64")]
    assert!(
        candidate
            .observations
            .lock()
            .unwrap()
            .compilations
            .iter()
            .any(|compile| compile.code_id == generation && compile.fused),
        "the chain must actually start after SubImm's first lowering operation"
    );
    #[cfg(target_arch = "aarch64")]
    let probes = &[
        (
            "JSON.stringify([fusedImmediateWork(12, 2), coercions]);",
            "[18,0]",
        ),
        (
            "JSON.stringify([fusedImmediateWork(numericBox, 2), coercions]);",
            "[18,1]",
        ),
        (
            "coercionThrows = true; try { fusedImmediateWork(numericBox, 2); } catch (error) { JSON.stringify([error, coercions]); }",
            "[17,2]",
        ),
    ];
    // x86 executes the retained numeric stream. Its coercing misses replay
    // the entered opcode and have a separate exact-attempt oracle below.
    #[cfg(target_arch = "x86_64")]
    let probes = &[(
        "JSON.stringify([fusedImmediateWork(12, 2), coercions]);",
        "[18,0]",
    )];
    for &(source, expected) in probes {
        let reference = oracle.probe(source);
        assert_eq!(reference.completion, expected);
        assert_eq!(
            reference
                .ticks
                .iter()
                .filter(|tick| tick.name == "fusedImmediateWork" && tick.op == Op::SubImm)
                .count(),
            1
        );
        let actual = candidate.probe(source);
        assert_exact(&reference, &actual, &["fusedImmediateWork", "coercionWork"]);
        assert_oracle_layout(
            &reference,
            &candidate,
            &["fusedImmediateWork", "coercionWork"],
        );
        assert_native(&actual, &["fusedImmediateWork", "coercionWork"]);
        assert_eq!(
            candidate.current("fusedImmediateWork", NativeFrameKind::Baseline),
            generation
        );
        if expected == "[17,2]" {
            assert!(
                !reference
                    .ticks
                    .iter()
                    .any(|tick| tick.name == "fusedImmediateWork" && tick.op == Op::Mul)
            );
        }
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn numeric_misses_credit_the_native_attempt_and_exact_interpreter_replay() {
    use otter_runtime::JitDebugTier;
    use otter_vm::native_abi::{ExitAction, ExitReason};

    let setup = format!(
        "{FUSION_SETUP}\n\
         function fusedBridge(a, b, c) {{ return fusedWork(a, b, c); }}\n\
         function immediateBridge(a, b) {{ return fusedImmediateWork(a, b); }}\n\
         for (let warm = 0; warm < 5000; warm++) {{\n\
           fusedBridge(12, 3, 2); immediateBridge(12, 2);\n\
         }}\ncoercions = 0;"
    );
    for (name, bridge, op, numeric, coercing) in [
        (
            "fusedWork",
            "fusedBridge",
            Op::Sub,
            "fusedBridge(12, 3, 2);",
            "fusedBridge(numericBox, 3, 2)",
        ),
        (
            "fusedImmediateWork",
            "immediateBridge",
            Op::SubImm,
            "immediateBridge(12, 2);",
            "immediateBridge(numericBox, 2)",
        ),
    ] {
        for throws in [false, true] {
            // Each miss starts with a fresh, warmed current generation. Its
            // exit may invalidate that generation, so a following miss must
            // not accidentally measure an interpreter-only replacement path.
            let mut oracle = Harness::new(&setup, Policy::Oracle);
            let mut candidate = Harness::new(&setup, Policy::Template);
            candidate.admit_with_calls("fusedWork(12, 3, 2); fusedImmediateWork(12, 2); fusedBridge(12, 3, 2); immediateBridge(12, 2);");
            let fid = candidate.block(name).id;
            let generation = candidate.current(name, NativeFrameKind::Baseline);
            let reference_hit = oracle.probe(numeric);
            let hit = candidate.probe(numeric);
            assert_exact(&reference_hit, &hit, &[name, bridge]);
            assert_native(&hit, &[name, bridge]);
            assert_eq!(
                candidate.current(name, NativeFrameKind::Baseline),
                generation
            );
            let before = candidate
                .runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .find(|entry| entry.code_object_id == generation)
                .unwrap();
            let source = if throws {
                format!(
                    "coercionThrows = true; try {{ {coercing}; }} \
                     catch (error) {{ JSON.stringify([error, coercions]); }}"
                )
            } else {
                format!("JSON.stringify([{coercing}, coercions]);")
            };
            let reference = oracle.probe(&source);
            assert_eq!(
                reference.completion,
                if throws { "[17,1]" } else { "[18,1]" }
            );
            let actual = candidate.probe(&source);
            assert_eq!(actual.completion, reference.completion);
            assert_oracle_layout(&reference, &candidate, &[name, bridge, "coercionWork"]);
            assert_exact(&reference, &actual, &[bridge, "coercionWork"]);
            assert_native(&actual, &[bridge, "coercionWork"]);

            let expected_ticks: Vec<_> = reference
                .ticks
                .iter()
                .filter(|tick| tick.name == name)
                .collect();
            let failed: Vec<_> = expected_ticks
                .iter()
                .enumerate()
                .filter(|(_, tick)| tick.op == op)
                .collect();
            assert_eq!(failed.len(), 1, "one attempted source arithmetic opcode");
            let (failed_index, failed_tick) = failed[0];
            let native_prefix = (failed_index + 1) as u64;
            let replay: Vec<_> = actual
                .ticks
                .iter()
                .filter(|tick| tick.name == name)
                .collect();
            let identity = |ticks: &[&Tick]| {
                ticks
                    .iter()
                    .map(|tick| (tick.byte_pc, tick.op))
                    .collect::<Vec<_>>()
            };
            assert_eq!(identity(&replay), identity(&expected_ticks[failed_index..]));
            assert_eq!(
                count(&actual.work, name),
                native_prefix + replay.len() as u64,
                "one native prefix plus the exact interpreter-dispatched suffix"
            );
            assert_eq!(
                count(&actual.work, name),
                count(&reference.traced, name) + 1
            );
            assert_eq!(actual.checkpoints.len(), 1);
            assert_eq!(actual.checkpoints[0].phase, 401);
            assert_eq!(
                count(&actual.checkpoints[0].work, name),
                native_prefix + count(&actual.checkpoints[0].traced, name)
            );
            assert_eq!(
                count(&actual.checkpoints[0].work, name),
                count(&reference.checkpoints[0].traced, name) + 1
            );

            let exits: Vec<_> = actual
                .events
                .iter()
                .filter_map(|event| match event {
                    JitDebugEvent::EnteredGenerationDeopt {
                        callee_function_id,
                        callee_code_object_id,
                        callee_tier,
                        callee_resume_pc,
                        exit_reason,
                        exit_action,
                    } if *callee_function_id == fid => Some((
                        *callee_code_object_id,
                        *callee_tier,
                        *callee_resume_pc,
                        *exit_reason,
                        *exit_action,
                    )),
                    _ => None,
                })
                .collect();
            assert_eq!(exits.len(), 1, "one exact-generation native source miss");
            let (exit_generation, tier, pc, reason, action) = exits[0];
            assert_eq!(exit_generation, generation);
            assert_eq!(tier, JitDebugTier::Template);
            assert_eq!(reason, ExitReason::TypeMismatch);
            assert_eq!(action, ExitAction::Recompile);
            assert_eq!(candidate.block(name).op_at(pc as usize), Some(op));
            let observation = candidate.observations.lock().unwrap();
            let byte_pc = observation.layouts[&fid]
                .iter()
                .filter(|(_, opcode)| **opcode == op)
                .map(|(&byte_pc, _)| byte_pc)
                .collect::<Vec<_>>();
            assert_eq!(byte_pc, vec![failed_tick.byte_pc]);
            drop(observation);

            let snapshots = candidate.runtime.jit_code_generation_snapshot();
            let after = snapshots
                .iter()
                .find(|entry| entry.code_object_id == generation)
                .unwrap();
            assert_eq!(after.generated_entries - before.generated_entries, 1);
            assert_eq!(after.generated_deopts - before.generated_deopts, 1);
            let installed: Vec<_> = snapshots
                .iter()
                .filter(|entry| {
                    entry.function_id == fid
                        && entry.tier == NativeFrameKind::Baseline
                        && entry.lifecycle == CodeLifetimeState::Installed
                        && entry.linked
                })
                .collect();
            if after.linked {
                assert_eq!(after.lifecycle, CodeLifetimeState::Installed);
                assert_eq!(installed.len(), 1);
                assert_eq!(installed[0].code_object_id, generation);
            } else {
                assert!(matches!(
                    after.lifecycle,
                    CodeLifetimeState::Invalid | CodeLifetimeState::Retired
                ));
                assert!(
                    installed.is_empty(),
                    "the exited generation cannot resurrect"
                );
            }
        }
    }
}

const ADD_REENTRY_SETUP: &str = r#"
let addCoercions = 0, addThrows = false;
function addHook() {
    addCoercions++; sourceCheckpoint(601);
    if (addThrows) throw 17;
    return 12;
}
const addBox = {valueOf: addHook};
function reentryAdd(a, b, c) { return (a + b) * c; }
for (let warm = 0; warm < 5000; warm++) {
    addHook(); reentryAdd(12, 3, 2);
}
addCoercions = 0;
"#;

#[test]
fn a_committed_add_helper_publishes_its_prefix_before_reentry_on_both_backends() {
    let mut oracle = Harness::new(ADD_REENTRY_SETUP, Policy::Oracle);
    let mut candidate = Harness::new(ADD_REENTRY_SETUP, Policy::Template);
    let generation = candidate.current("reentryAdd", NativeFrameKind::Baseline);
    for (source, expected) in [
        (
            "JSON.stringify([reentryAdd(12, 3, 2), addCoercions]);",
            "[30,0]",
        ),
        (
            "JSON.stringify([reentryAdd(addBox, 3, 2), addCoercions]);",
            "[30,1]",
        ),
        (
            "addThrows = true; try { reentryAdd(addBox, 3, 2); } catch (error) { JSON.stringify([error, addCoercions]); }",
            "[17,2]",
        ),
    ] {
        let reference = oracle.probe(source);
        assert_eq!(reference.completion, expected);
        assert!(
            reference
                .ticks
                .iter()
                .any(|tick| tick.name == "reentryAdd" && tick.op == Op::Add)
        );
        let actual = candidate.probe(source);
        assert_exact(&reference, &actual, &["reentryAdd", "addHook"]);
        assert_oracle_layout(&reference, &candidate, &["reentryAdd", "addHook"]);
        assert_native(&actual, &["reentryAdd", "addHook"]);
        assert_eq!(
            candidate.current("reentryAdd", NativeFrameKind::Baseline),
            generation
        );
        assert!(!actual.events.iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail { function_name, .. } if function_name == "reentryAdd"
        )));
        let fid = candidate.block("reentryAdd").id;
        assert!(!actual.events.iter().any(|event| matches!(
            event,
            JitDebugEvent::EnteredGenerationDeopt { callee_function_id, .. } if *callee_function_id == fid
        )));
        if expected != "[30,0]" {
            assert_eq!(actual.checkpoints.len(), 1);
            assert_eq!(actual.checkpoints[0].phase, 601);
        }
        if expected == "[17,2]" {
            assert!(
                !reference
                    .ticks
                    .iter()
                    .any(|tick| tick.name == "reentryAdd" && tick.op == Op::Mul)
            );
        }
    }
}

const METHOD_SETUP: &str = r#"
function methodFirst(x) { return x + 1; }
function methodSecond(x) { sourceCheckpoint(501); return x + 2; }
const receiver = {run: methodFirst};
function methodWork(object, x) { sourceCheckpoint(500); return object.run(x); }
for (let warm = 0; warm < 5000; warm++) { methodFirst(5); methodSecond(5); }
for (let warm = 0; warm < 5000; warm++) methodWork(receiver, 5);
"#;

#[test]
fn inlined_method_identity_miss_never_credits_the_unentered_leaf() {
    for (source, expected) in [
        ("methodWork(receiver, 5);", "6"),
        ("receiver.run = methodSecond; methodWork(receiver, 5);", "7"),
        (
            "receiver.run = 0; try { methodWork(receiver, 5); } catch (error) { error.name; }",
            "TypeError",
        ),
    ] {
        // Publishing a new method target retires the immutable caller guard
        // chain after its active invocation completes. Each case therefore
        // starts with its own real, warmed generation containing methodFirst.
        let mut oracle = Harness::new(METHOD_SETUP, Policy::Oracle);
        let mut candidate = Harness::new(METHOD_SETUP, Policy::Template);
        candidate.admit_with_calls("methodWork(receiver, 5); methodWork(receiver, 5);");
        let block = candidate.block("methodWork");
        let generation = candidate.current("methodWork", NativeFrameKind::Baseline);
        let epoch = block.feedback_epoch();
        let before = candidate
            .runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .find(|entry| entry.code_object_id == generation)
            .unwrap();
        assert_eq!(before.generated_deopts, 0);
        #[cfg(target_arch = "aarch64")]
        let first = candidate.block("methodFirst").id;
        #[cfg(target_arch = "aarch64")]
        assert!(
            candidate
                .observations
                .lock()
                .unwrap()
                .compilations
                .iter()
                .any(|compile| compile.code_id == generation && compile.spliced.contains(&first)),
            "the actual Template code object must splice methodFirst"
        );
        let reference = oracle.probe(source);
        assert_eq!(reference.completion, expected);
        let actual = candidate.probe(source);
        assert_exact(
            &reference,
            &actual,
            &["methodWork", "methodFirst", "methodSecond"],
        );
        assert_oracle_layout(
            &reference,
            &candidate,
            &["methodWork", "methodFirst", "methodSecond"],
        );
        assert_native(&actual, &["methodWork", "methodFirst", "methodSecond"]);
        let after = candidate
            .runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .find(|entry| entry.code_object_id == generation)
            .expect("the exact entry-cell record survives generation retirement");
        assert_eq!(after.generated_entries, before.generated_entries + 1);
        assert_eq!(after.generated_deopts, 0);
        assert!(!actual.events.iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail { function_id, .. }
                | JitDebugEvent::InlineDeoptFrame { function_id, .. }
                if *function_id == block.id
        )));
        assert!(!actual.events.iter().any(|event| matches!(
            event,
            JitDebugEvent::EnteredGenerationDeopt { callee_function_id, .. }
                if *callee_function_id == block.id
        )));
        assert!(!actual.events.iter().any(|event| matches!(
            event,
            JitDebugEvent::CompilePrepared { function_id, .. }
                if *function_id == block.id
        )));
        if expected == "7" {
            assert!(block.feedback_epoch() > epoch);
            assert!(!after.linked);
            assert!(matches!(
                after.lifecycle,
                CodeLifetimeState::Invalid | CodeLifetimeState::Retired
            ));
            assert_eq!(count(&actual.work, "methodFirst"), 0);
            assert!(count(&actual.work, "methodSecond") > 0);
        } else {
            assert_eq!(block.feedback_epoch(), epoch);
            assert_eq!(after.lifecycle, CodeLifetimeState::Installed);
            assert!(after.linked);
            assert_eq!(
                candidate.current("methodWork", NativeFrameKind::Baseline),
                generation
            );
            if expected == "TypeError" {
                assert_eq!(count(&actual.work, "methodFirst"), 0);
                assert_eq!(count(&actual.work, "methodSecond"), 0);
            }
        }
    }
}

const OSR_SETUP: &str = r#"
function osrWork(count) {
    let sum = 0;
    for (let index = 0; index < count; index++) {
        sourceCheckpoint(index);
        sum += index;
    }
    return sum;
}
"#;

#[test]
fn template_osr_counts_the_interpreted_prefix_and_native_remainder_once() {
    let mut oracle = Harness::new(OSR_SETUP, Policy::Oracle);
    let mut candidate = Harness::new(OSR_SETUP, Policy::Template);
    let before = candidate.runtime.execution_stats();
    let reference = oracle.probe("osrWork(4096);");
    assert_eq!(reference.completion, "8386560");
    let actual = candidate.probe("osrWork(4096);");
    assert_eq!(actual.completion, reference.completion);
    assert_eq!(
        count(&actual.work, "osrWork"),
        count(&reference.traced, "osrWork")
    );
    assert_eq!(actual.checkpoints.len(), reference.checkpoints.len());
    let mut captured_points = 0;
    for (actual, expected) in actual.checkpoints.iter().zip(&reference.checkpoints) {
        assert_eq!(actual.phase, expected.phase);
        // No compiler request owns a capture before initial OSR admission.
        // The final total still includes those earlier interpreted attempts.
        if actual.work.contains_key("osrWork") {
            captured_points += 1;
            assert_eq!(
                count(&actual.work, "osrWork"),
                count(&expected.traced, "osrWork")
            );
        }
    }
    assert!(captured_points > 100);
    assert_oracle_layout(&reference, &candidate, &["osrWork"]);
    assert!(count(&actual.traced, "osrWork") > 0);
    assert!(count(&actual.traced, "osrWork") < count(&reference.traced, "osrWork"));
    assert!(candidate.runtime.execution_stats().jit_osr_attempts > before.jit_osr_attempts);
    let fid = candidate.block("osrWork").id;
    assert!(
        candidate
            .observations
            .lock()
            .unwrap()
            .compilations
            .iter()
            .any(|compile| compile.fid == fid
                && compile.succeeded
                && compile.osr_pc.is_some()
                && compile.tier == NativeFrameKind::Baseline)
    );
    let halfway = &actual.checkpoints[2048];
    let last = actual.checkpoints.last().unwrap();
    assert_eq!(
        count(&halfway.traced, "osrWork"),
        count(&last.traced, "osrWork"),
        "the OSR remainder must not be an interpreter polling loop"
    );
}

#[test]
fn optimizing_osr_stops_source_work_at_the_exact_completed_iteration() {
    let mut oracle = Harness::new(OSR_SETUP, Policy::Oracle);
    let mut candidate = Harness::new(OSR_SETUP, Policy::Production);
    let before = candidate.runtime.execution_stats();
    let reference = oracle.probe("osrWork(4096);");
    let actual = candidate.probe("osrWork(4096);");
    assert_eq!(actual.completion, reference.completion);
    assert_oracle_layout(&reference, &candidate, &["osrWork"]);
    let fid = candidate.block("osrWork").id;
    let compilation = candidate
        .observations
        .lock()
        .unwrap()
        .compilations
        .iter()
        .find(|compile| {
            compile.fid == fid
                && compile.tier == NativeFrameKind::Optimizing
                && compile.succeeded
                && compile.osr_pc.is_some()
        })
        .expect("an actual optimizing OSR body")
        .clone();
    let phase = compilation
        .last_phase
        .expect("OSR follows an observed iteration");
    let oracle_checkpoint = &reference.checkpoints[phase as usize];
    assert_eq!(oracle_checkpoint.phase, phase);
    // The compile request runs after the entered backedge of this iteration.
    // Recover that position from the independent interpreter oracle, not the
    // emitted instruction span or the measured source-work counter.
    let block = candidate.block("osrWork");
    let header_pc = compilation.osr_pc.unwrap();
    let latch_pc = block
        .control_flow()
        .loop_latch(header_pc)
        .expect("the verified OSR header has a backedge");
    assert_eq!(block.op_at(latch_pc as usize), Some(Op::Jump));
    let observation = candidate.observations.lock().unwrap();
    let layout = &observation.layouts[&fid];
    let header_byte_pc = *layout.keys().nth(header_pc as usize).unwrap();
    let latch_byte_pc = *layout.keys().nth(latch_pc as usize).unwrap();
    drop(observation);
    let latch = reference.ticks[oracle_checkpoint.trace_len..]
        .windows(2)
        .position(|ticks| {
            ticks[0].name == "osrWork"
                && ticks[0].op == Op::Jump
                && ticks[0].byte_pc == latch_byte_pc
                && ticks[1].fid == ticks[0].fid
                && ticks[1].depth == ticks[0].depth
                && ticks[1].byte_pc == header_byte_pc
        })
        .expect("observed iteration's actual Jump reaches the verified OSR header")
        + oracle_checkpoint.trace_len;
    let expected_prefix = reference.ticks[..=latch]
        .iter()
        .filter(|tick| tick.name == "osrWork")
        .count() as u64;
    assert_eq!(
        compilation.source_work, expected_prefix,
        "exact pre-Graph interpreter/Template attempts"
    );
    let after = candidate.runtime.execution_stats();
    assert!(after.jit_optimized_osr_entries > before.jit_optimized_osr_entries);
    let native_points: Vec<_> = actual
        .checkpoints
        .iter()
        .filter(|point| point.optimized_osr_entries > before.jit_optimized_osr_entries)
        .collect();
    assert!(
        native_points.len() > 100,
        "actual Graph body must execute many remaining iterations"
    );
    for point in native_points {
        assert_eq!(
            count(&point.work, "osrWork"),
            expected_prefix,
            "Graph is uncharged at iteration {}",
            point.phase
        );
    }
    let suffix = actual.ticks[compilation.trace_len..]
        .iter()
        .filter(|tick| tick.name == "osrWork")
        .count() as u64;
    assert_eq!(
        count(&actual.work, "osrWork"),
        expected_prefix + suffix,
        "a cold post-loop interpreter suffix remains real source work"
    );
}

#[test]
fn an_isolated_current_optimizing_entry_adds_no_source_work() {
    // A non-looping warm main cannot splice or OSR over the measured calls.
    let setup = format!(
        "function graphWork(x) {{ return x + 1; }}\n{}",
        "graphWork(7);\n".repeat(4000)
    );
    let mut candidate = Harness::new(&setup, Policy::Production);
    let generation = candidate.current("graphWork", NativeFrameKind::Optimizing);
    let before_template_entries: u64 = candidate
        .runtime
        .jit_code_generation_snapshot()
        .iter()
        .filter(|generation| generation.tier == NativeFrameKind::Baseline)
        .map(|generation| generation.generated_entries)
        .sum();
    let before = candidate.runtime.execution_stats();
    let actual = candidate.probe("graphWork(7);");
    assert_eq!(actual.completion, "8");
    assert_native(&actual, &["graphWork"]);
    assert_eq!(count(&actual.work, "graphWork"), 0);
    let after = candidate.runtime.execution_stats();
    assert_eq!(
        after.jit_optimized_osr_entries,
        before.jit_optimized_osr_entries
    );
    assert_eq!(after.jit_code_generations, before.jit_code_generations);
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
    let after_template_entries: u64 = candidate
        .runtime
        .jit_code_generation_snapshot()
        .iter()
        .filter(|generation| generation.tier == NativeFrameKind::Baseline)
        .map(|generation| generation.generated_entries)
        .sum();
    assert_eq!(
        after_template_entries, before_template_entries,
        "no Template generation substitutes for the proved Graph entry"
    );
    assert_eq!(
        candidate.current("graphWork", NativeFrameKind::Optimizing),
        generation
    );
}
