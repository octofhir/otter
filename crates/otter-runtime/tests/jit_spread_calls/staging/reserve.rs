//! Native spread reserve interval with exact own activation and moving operands.
//!
//! # Contents
//! - Ordinary Template/Graph admission with an unchanged Array iterator proof.
//! - A source and separate child moved by the next append reserve allocation.
//! - Exact actual values, source frames, return sites and no-replay observations.
//!
//! # Invariants
//! - The arm runs after the hidden carrier shell exists and before SpreadAppend.
//! - Only offsets and owned diagnostic data escape an observation callback.
//! - The interval contains exactly one runtime cell allocation and one minor GC.
//! - Carrier relocation itself belongs to the VM reserve test; this runtime
//!   boundary proves source/child movement and the carrier's actual contents.
//!
//! # See also
//! - `array::append_plain_dense` and `iteration_protocol::Proof::current`.
//! - `call_trampoline::tests::spread_staging` proves pending actual root movement.

use super::*;
use otter_runtime::{GcPauseCapture, GcPauseKind, GcPauseOutcome, GcPauseTrigger};
use otter_vm::array;

#[allow(dead_code)]
#[path = "../../support/return_sites.rs"]
mod return_sites;

const RESERVE_MODULE: &str = "jit-spread-reserve-setup.js";
const RESERVE_SETUP: &str = r#"let reserveEffects = 0;
let reserveResult;
let reserveChild;
function reserveSink(source, child) { return reserveObserve(source, child); }
function reserveWrapper() { reserveEffects++; return reserveSink(...reserveArm(reserveSource())); }
function reserveWarmBatch() { for (let i = 0; i < 16; i++) reserveWrapper(); }
"#;
const SINK_LINE: &str =
    "function reserveSink(source, child) { return reserveObserve(source, child); }";
const WRAPPER_LINE: &str = "function reserveWrapper() { reserveEffects++; return reserveSink(...reserveArm(reserveSource())); }";

struct Point {
    source: u32,
    child: u32,
    minor: u64,
    full: u64,
    allocations: u64,
    sources: String,
    generations: Vec<JitCodeGenerationSnapshot>,
}

struct Interval {
    before: Point,
    after: Point,
    capture: GcPauseCapture,
}

fn observe(ctx: &mut RuntimeNativeCtx<'_>, source: RuntimeValue, child: RuntimeValue) -> Point {
    let source_array = source.as_array().expect("source actual");
    let child_array = child.as_array().expect("child actual");
    assert_ne!(source_array.offset(), child_array.offset());
    assert_eq!(array::len(source_array, ctx.heap()), 2);
    assert_eq!(array::len(child_array, ctx.heap()), 0);
    assert_eq!(
        array::get(source_array, ctx.heap(), 0).to_bits(),
        source.to_bits()
    );
    assert_eq!(
        array::get(source_array, ctx.heap(), 1).to_bits(),
        child.to_bits()
    );
    let stats = ctx.heap_mut().gc_stats();
    let point = (
        source_array.offset(),
        child_array.offset(),
        stats.minor_gc_cycles,
        stats.gc_cycles,
        stats.by_type.iter().map(|row| row.alloc_count_total).sum(),
    );
    let sources = ctx.capture_call_sites_json(ctx.execution_context().unwrap(), 0, 64);
    let generations = ctx.interp_mut().jit_code_generation_snapshot();
    Point {
        source: point.0,
        child: point.1,
        minor: point.2,
        full: point.3,
        allocations: point.4,
        sources,
        generations,
    }
}

fn assert_active(
    point: &Point,
    generation: &JitCodeGenerationSnapshot,
    name: &str,
    source_line: &str,
) {
    let frames: serde_json::Value = serde_json::from_str(&point.sources).unwrap();
    let frames = frames.as_array().unwrap();
    assert!(frames.len() < 64, "complete source capture");
    let own: Vec<_> = frames
        .iter()
        .filter(|frame| frame["functionName"] == name)
        .collect();
    assert_eq!(own.len(), 1, "one own {name} source frame");
    assert_eq!(own[0]["scriptName"], RESERVE_MODULE);
    assert_eq!(own[0]["sourceLine"].as_str().unwrap().trim(), source_line);
    let active = point
        .generations
        .iter()
        .find(|active| active.code_object_id == generation.code_object_id)
        .unwrap();
    assert_eq!(active.function_id, generation.function_id);
    assert_eq!(active.tier, generation.tier);
    assert_eq!(active.lifecycle, CodeLifetimeState::Installed);
    assert!(active.current_entry && active.linked && active.active_count > 0);
}

fn assert_reserve_artifact(artifacts: &JitArtifactBatch, generation: &JitCodeGenerationSnapshot) {
    assert!(!artifacts.truncated());
    let bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
        .unwrap();
    if let Some(root) = std::env::var_os("OTTER_ACTUAL_PROOF_ARTIFACTS") {
        let directory = std::path::PathBuf::from(root).join(format!(
            "reserve-{:?}-{}",
            generation.tier, generation.code_object_id
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("generation.txt"), format!("{generation:#?}")).unwrap();
        std::fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec_pretty(bundle.manifest()).unwrap(),
        )
        .unwrap();
        for file in bundle.files() {
            std::fs::write(directory.join(file.name().as_str()), file.contents()).unwrap();
        }
    }
    assert_eq!(bundle.manifest().function_id(), generation.function_id);
    assert_eq!(bundle.manifest().module(), RESERVE_MODULE);
    assert_eq!(bundle.manifest().entry(), JitDebugTarget::Entry);
    let bytecode = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Bytecode)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let array = bytecode.find("NewArray").unwrap();
    let append = bytecode.find("SpreadAppend").unwrap();
    let spread_call = bytecode.find("CallSpread").unwrap();
    assert!(
        array < append && append < spread_call,
        "carrier precedes argument evaluation and append: {bytecode}"
    );
    assert_eq!(bytecode.matches("NewArray").count(), 1);
    assert_eq!(bytecode.matches("SpreadAppend").count(), 1);
    assert_eq!(bytecode.matches("CallSpread").count(), 1);
    // Neither nested source/arm call is a spread; both are ordinary Call
    // instructions between the carrier allocation and the append instruction.
    let argument_calls = bytecode[array..append]
        .lines()
        .filter(|line| {
            line.split_whitespace()
                .any(|word| matches!(word, "Call" | "CallWithThis"))
        })
        .count();
    assert_eq!(
        argument_calls, 2,
        "source then arm run after carrier creation: {bytecode}"
    );
    let json = |name| {
        serde_json::from_slice::<serde_json::Value>(bundle.file(name).unwrap().contents()).unwrap()
    };
    let map = json(JitArtifactFileName::CodeMap);
    assert_eq!(
        map["callEntryOffset"].as_u64(),
        generation.call_entry_offset.map(u64::from)
    );
    assert!(generation.call_entry_offset.is_some());
    let relocations = json(JitArtifactFileName::Relocations);
    let stage: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| {
            relocation["target"]["kind"] == "runtimeStub"
                && relocation["target"]["name"] == "jit_stage_spread"
        })
        .collect();
    let loads: Vec<_> = stage
        .iter()
        .filter(|site| site["form"] != "literalWord")
        .collect();
    assert_eq!(loads.len(), 1, "one executable spread staging site");
    let load = loads[0];
    let stage_offset = load["startOffset"].as_u64().unwrap();
    match load["form"].as_str() {
        Some("literalLoad") => {
            assert_eq!(stage.len(), 2, "one load and its one literal word");
            let literal = stage
                .iter()
                .find(|site| site["form"] == "literalWord")
                .unwrap();
            let literal_offset = literal["startOffset"].as_u64().unwrap();
            assert_eq!(load["endOffset"].as_u64(), Some(stage_offset + 4));
            assert_eq!(literal["endOffset"].as_u64(), Some(literal_offset + 8));
            let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
            let at = usize::try_from(stage_offset).unwrap();
            let instruction = u32::from_le_bytes(code[at..at + 4].try_into().unwrap());
            assert_eq!(instruction & 0xff00_0000, 0x5800_0000, "64-bit LDR literal");
            let register = instruction & 31;
            assert_eq!(load["register"].as_u64(), Some(u64::from(register)));
            let displacement = ((((instruction >> 5) & 0x7ffff) as i32) << 13) >> 11;
            assert_eq!(
                i64::try_from(stage_offset).unwrap() + i64::from(displacement),
                i64::try_from(literal_offset).unwrap(),
                "the executable load reads this exact captured pool word"
            );
            assert_eq!(
                u32::from_le_bytes(code[at + 4..at + 8].try_into().unwrap()),
                0xd63f_0000 | (register << 5),
                "the loaded staging entry is called immediately"
            );
            // Template names its pool explicitly; Graph includes the pool in
            // its one structural tail after all emitted node regions.
            let structural_kind = if generation.tier == NativeFrameKind::Baseline {
                "literalPool"
            } else {
                "out-of-line"
            };
            let pool: Vec<_> = map["regions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|region| {
                    region["kind"] == structural_kind
                        && region["startOffset"].as_u64().unwrap() <= literal_offset
                        && region["endOffset"].as_u64().unwrap() >= literal_offset + 8
                })
                .collect();
            assert_eq!(pool.len(), 1, "the address word is data, not another call");
            assert!(!map["regions"].as_array().unwrap().iter().any(|region| {
                region["kind"] == "instruction"
                    && region["startOffset"].as_u64().unwrap() < literal_offset + 8
                    && region["endOffset"].as_u64().unwrap() > literal_offset
            }));
        }
        None => assert_eq!(stage.len(), 1, "one immediate address materialization"),
        form => panic!("unexpected staging relocation form: {form:?}"),
    }
    let owners: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["startOffset"]
                    .as_u64()
                    .is_some_and(|start| start <= stage_offset)
                && region["endOffset"]
                    .as_u64()
                    .is_some_and(|end| stage_offset < end)
        })
        .collect();
    assert_eq!(owners.len(), 1);
    let mut source = bytecode
        .lines()
        .find(|line| line.split_whitespace().any(|word| word == "CallSpread"))
        .unwrap()
        .split_whitespace();
    let logical_pc: u64 = source.next().unwrap().parse().unwrap();
    let byte_pc: u64 = source
        .next()
        .unwrap()
        .strip_prefix("byte=")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(owners[0]["logicalPc"].as_u64(), Some(logical_pc));
    assert_eq!(owners[0]["bytePc"].as_u64(), Some(byte_pc));
    assert_eq!(
        owners[0]["functionId"].as_u64(),
        Some(u64::from(generation.function_id))
    );
    let metadata = return_sites::assert_sites(bundle);
    assert!(
        metadata["returnSites"]
            .as_array()
            .unwrap()
            .iter()
            .any(|site| metadata["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["id"] == site["safepointId"]
                    && record["callPc"] == owners[0]["logicalPc"])),
        "the staged spread call retains its actual return/source association"
    );
}

#[test]
fn native_spread_reserve_moves_source_and_child_without_replay() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let recording = Arc::new(AtomicBool::new(false));
        let armed = Arc::new(Mutex::new(None::<Point>));
        let intervals = Arc::new(Mutex::new(Vec::<Interval>::new()));
        let arm_count = Arc::new(AtomicUsize::new(0));
        let target_count = Arc::new(AtomicUsize::new(0));
        let installer = RuntimeExtensionInstaller::new({
            let recording = recording.clone();
            let armed = armed.clone();
            let intervals = intervals.clone();
            let arm_count = arm_count.clone();
            let target_count = target_count.clone();
            move |realm| {
                realm.install_native_global_call(
                    "reserveSource",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        |ctx: &mut RuntimeNativeCtx<'_>,
                         args: ValueWindow<'_>,
                         _state: ValueWindow<'_>| {
                            assert!(args.is_empty());
                            // This existing builder makes a private tagged slab first
                            // and the source shell last. Park its returned value before
                            // the next allocation; never retain that raw local across GC.
                            let source = ctx
                                .array_from_elements([
                                    RuntimeValue::boolean(true),
                                    RuntimeValue::boolean(true),
                                ])
                                .map_err(|error| RuntimeNativeError::Error {
                                    message: error.to_string(),
                                })?;
                            ctx.scope(|mut scope| {
                                let source = scope.value(RuntimeValue::array(source));
                                let child = scope.array(0)?;
                                scope.set_index(source, 0, source)?;
                                scope.set_index(source, 1, child)?;
                                Ok(scope.finish(source))
                            })
                        },
                    )),
                )?;
                let arm_recording = recording.clone();
                let arm_pending = armed.clone();
                let arms = arm_count.clone();
                realm.install_native_global_call(
                    "reserveArm",
                    1,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              args: ValueWindow<'_>,
                              _state: ValueWindow<'_>| {
                            assert_eq!(args.len(), 1);
                            let source = args[0];
                            if arm_recording.load(Ordering::Relaxed) {
                                arms.fetch_add(1, Ordering::Relaxed);
                                let array = source.as_array().unwrap();
                                let child = array::get(array, ctx.heap(), 1);
                                // Use the collector's existing safe stopped inspection
                                // boundary; never reconstruct or dereference a header in
                                // this runtime integration target.
                                assert!(
                                    !ctx.heap().is_sweeping(),
                                    "young-space observation requires completed sweeping"
                                );
                                let offsets = [array.offset(), child.as_array().unwrap().offset()];
                                let mut spaces = [None; 2];
                                let mut matches = [0; 2];
                                ctx.heap().for_each_live_body(|space, body| {
                                    for (index, offset) in offsets.iter().enumerate() {
                                        if body.raw().0 == *offset {
                                            spaces[index] = Some(space);
                                            matches[index] += 1;
                                        }
                                    }
                                });
                                assert_eq!(matches, [1, 1]);
                                assert_eq!(spaces, [Some(otter_gc::page::SpaceKind::NewFrom); 2]);
                                let before = observe(ctx, source, child);
                                assert!(arm_pending.lock().unwrap().replace(before).is_none());
                                ctx.heap_mut().start_gc_pause_capture(4).unwrap();
                                // Numeric external strides still cover warm/preparation.
                                // The tested reserve interval deliberately arms stride1.
                                ctx.heap_mut().set_gc_stress(1, false);
                            }
                            Ok(source)
                        },
                    )),
                )?;
                let target_recording = recording.clone();
                let target_pending = armed.clone();
                let target_intervals = intervals.clone();
                let targets = target_count.clone();
                realm.install_native_global_call(
                    "reserveObserve",
                    2,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              args: ValueWindow<'_>,
                              _state: ValueWindow<'_>| {
                            assert_eq!(args.len(), 2);
                            if target_recording.load(Ordering::Relaxed) {
                                targets.fetch_add(1, Ordering::Relaxed);
                                let capture = ctx.heap_mut().take_gc_pause_capture().unwrap();
                                let after = observe(ctx, args[0], args[1]);
                                let before = target_pending
                                    .lock()
                                    .unwrap()
                                    .take()
                                    .expect("one arm before target");
                                target_intervals.lock().unwrap().push(Interval {
                                    before,
                                    after,
                                    capture,
                                });
                            }
                            Ok(args[0])
                        },
                    )),
                )
            }
        });
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(installer)
            .build()
            .unwrap();
        let trace = Arc::new(Mutex::new(Trace::default()));
        runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        let mut setup = runtime
            .run_script(SourceInput::from_javascript(RESERVE_SETUP), RESERVE_MODULE)
            .unwrap();
        let mut artifacts = setup.take_jit_artifacts().unwrap();
        let tier = if selection == JitSelection::Template {
            NativeFrameKind::Baseline
        } else {
            NativeFrameKind::Optimizing
        };
        let current_pair = |runtime: &Runtime| -> Option<Vec<JitCodeGenerationSnapshot>> {
            let trace = trace.lock().unwrap();
            ["reserveWrapper", "reserveSink"]
                .into_iter()
                .map(|name| current(runtime, *trace.names.get(name)?, tier))
                .collect()
        };
        let mut admitted = None;
        for batch in 0..512 {
            artifacts = acknowledge_pending(&mut runtime, artifacts, batch * 2);
            let before = current_pair(&runtime);
            let mut warm = runtime
                .run_script(
                    SourceInput::from_javascript("reserveWarmBatch();"),
                    &format!("reserve-warm-{batch}.js"),
                )
                .unwrap();
            artifacts = artifacts.merged(warm.take_jit_artifacts().unwrap());
            assert!(!artifacts.truncated());
            artifacts = acknowledge_pending(&mut runtime, artifacts, batch * 2 + 1);
            if let (Some(before), Some(after)) = (before, current_pair(&runtime))
                && before
                    .iter()
                    .zip(&after)
                    .all(|(before, after)| before.code_object_id == after.code_object_id)
            {
                admitted = Some(after);
                break;
            }
        }
        let generations = admitted
            .expect("both ordinary current generations stay stable across a complete warm batch");
        for generation in &generations {
            assert!(
                generation.call_entry_offset.is_some(),
                "own private-JS entry"
            );
            let bundle = artifacts
                .bundles()
                .iter()
                .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
                .expect("exact admitted wrapper/sink artifact");
            assert_eq!(bundle.manifest().function_id(), generation.function_id);
            assert_eq!(bundle.manifest().module(), RESERVE_MODULE);
            assert_eq!(bundle.manifest().entry(), JitDebugTarget::Entry);
            let map: serde_json::Value = serde_json::from_slice(
                bundle
                    .file(JitArtifactFileName::CodeMap)
                    .unwrap()
                    .contents(),
            )
            .unwrap();
            assert_eq!(
                map["callEntryOffset"].as_u64(),
                generation.call_entry_offset.map(u64::from)
            );
        }
        let generation = &generations[0];
        let sink = &generations[1];
        assert_reserve_artifact(&artifacts, generation);
        assert_eq!(runtime.jit_code_residency().pending_compilations, 0);
        recording.store(true, Ordering::Relaxed);
        trace.lock().unwrap().recording = true;
        let result = runtime.run_script(SourceInput::from_javascript("const reserveBefore = reserveEffects; reserveResult = reserveWrapper(); reserveChild = reserveResult[1]; reserveEffects - reserveBefore;"), "jit-spread-reserve-probe.js").unwrap();
        trace.lock().unwrap().recording = false;
        recording.store(false, Ordering::Relaxed);
        assert_eq!(result.completion_string(), "1");
        assert_eq!(arm_count.load(Ordering::Relaxed), 1);
        assert_eq!(target_count.load(Ordering::Relaxed), 1);
        assert!(armed.lock().unwrap().is_none());
        let trace = trace.lock().unwrap();
        assert!(trace.steps > 0);
        for generation in &generations {
            assert!(
                !trace.entered.contains(&generation.function_id),
                "both admitted bodies execute natively"
            );
            let after = current(&runtime, generation.function_id, tier).unwrap();
            assert_eq!(after.code_object_id, generation.code_object_id);
            assert_eq!(after.generated_deopts, generation.generated_deopts);
            assert_eq!(after.active_count, 0);
        }
        drop(trace);
        let report = result.jit_debug_report().unwrap();
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        assert!(!report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail { .. }
                | JitDebugEvent::EnteredGenerationDeopt { .. }
                | JitDebugEvent::InlineDeoptFrame { .. }
        )));
        let intervals = intervals.lock().unwrap();
        assert_eq!(intervals.len(), 1);
        let interval = &intervals[0];
        assert_active(&interval.before, generation, "reserveWrapper", WRAPPER_LINE);
        assert_active(&interval.after, generation, "reserveWrapper", WRAPPER_LINE);
        assert_active(&interval.after, sink, "reserveSink", SINK_LINE);
        assert_ne!(interval.before.source, interval.after.source);
        assert_ne!(interval.before.child, interval.after.child);
        assert_eq!(interval.after.minor, interval.before.minor + 1);
        assert_eq!(interval.after.full, interval.before.full);
        assert_eq!(
            interval.after.allocations,
            interval.before.allocations + 1,
            "only the append reservation allocates a GC cell"
        );
        assert!(!interval.capture.incomplete && !interval.capture.contains_split_phases);
        assert_eq!(interval.capture.dropped_records, 0);
        assert_eq!(interval.capture.records.len(), 1);
        let pause = &interval.capture.records[0];
        assert_eq!(pause.kind, GcPauseKind::Minor);
        assert_eq!(pause.trigger, GcPauseTrigger::Stress);
        assert_eq!(pause.outcome, GcPauseOutcome::Completed);
        assert_eq!(pause.minor_cycles_before, interval.before.minor);
        assert_eq!(pause.minor_cycles_after, interval.after.minor);
        assert_eq!(pause.full_cycles_before, interval.before.full);
        assert_eq!(pause.full_cycles_after, interval.after.full);
        drop(intervals);
        runtime.force_gc().unwrap();
        let retained = runtime.run_script(SourceInput::from_javascript("reserveResult[0] === reserveResult && reserveResult[1] === reserveChild && reserveChild.length === 0;"), "jit-spread-reserve-retained.js").unwrap();
        assert_eq!(retained.completion_string(), "true");
    }
}
