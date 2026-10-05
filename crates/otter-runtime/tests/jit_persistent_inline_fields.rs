//! Shape-owned inline and overflow fields through actual native execution.
//!
//! # Contents
//! - Equal byte displacements in two storage banks remain distinct.
//! - Growing the overflow suffix preserves the inline prefix and native hits.
//! - A native callback collects with children live in both field banks.
//! - Fresh bank children are published after probe compilation, and their
//!   exact rooted offsets both change during the measured collection.
//! - Current code identities, typed field metadata and complete probe traces.
//! - Template and Optimizing entries on both supported native architectures.
//! - The own emitted call and published caller position around a host collection.
//!
//! # Invariants
//! - Warmup uses normal production admission and no threshold overrides.
//! - Each fresh probe has no loop, compilation or interpreted subject dispatch.
//! - An independent interpreter supplies the exact observable result.
//! - Each probe retains its sole current native generation with no new compile
//!   or exit and no interpreter dispatch of the subject. Generated-entry counts
//!   measure Baseline promotion work only; Optimizing uses identity and trace.
//! - Private JavaScript host calls are proved from their own emitted call and
//!   published source frame, independently of typed VM-stub transition counts.
//! - Host callbacks collect owned observations without fixture assertions;
//!   all proof checks run after the private host boundary returns to Rust.
//!
//! # See also
//! - `otter_vm::object::FieldLocation` owns bank and relative index.
//! - `object::field_location` tests resident storage and moving collection.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitSelection, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind, STUB_JIT_CALL_GENERIC},
    object::FieldLocation,
};

#[path = "support/moving_children.rs"]
mod moving_children;

#[derive(Default)]
struct Trace {
    names: BTreeMap<u32, String>,
    recording: bool,
    count: usize,
    ticks: Vec<u32>,
}

#[derive(Clone, Copy)]
struct CollectingCall {
    function_id: u32,
    tier: NativeFrameKind,
    code_object_id: u64,
}

struct PublishedCaller {
    frames_json: Option<String>,
    generations: Vec<JitCodeGenerationSnapshot>,
}

struct CollectionObservation {
    before: PublishedCaller,
    after: PublishedCaller,
    children: Result<Vec<moving_children::ChildMotion>, String>,
}

#[derive(Default)]
struct CollectionFrames {
    observations: Vec<CollectionObservation>,
}

/// Join the own instruction region to its private-JS trampoline relocation and
/// the actual indirect call immediately following that address-bearing move.
fn collecting_call_artifact(bundle: &JitArtifactBundle, tier: NativeFrameKind) -> CollectingCall {
    let map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let calls: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["functionId"].as_u64() == Some(u64::from(bundle.manifest().function_id()))
                && region["operation"].as_str().is_some_and(|operation| {
                    if tier == NativeFrameKind::Optimizing {
                        operation.contains(" CallJs {")
                    } else {
                        operation.starts_with("Call {")
                    }
                })
        })
        .collect();
    assert_eq!(calls.len(), 1, "one own collecting call: {calls:?}");
    let call = calls[0];
    let start = call["startOffset"].as_u64().unwrap();
    let end = call["endOffset"].as_u64().unwrap();
    assert!(start < end, "collecting call emits native code");
    let relocations: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Relocations)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let links: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row["target"]["kind"] == "runtimeStub"
                && row["target"]["id"].as_u64() == Some(u64::from(STUB_JIT_CALL_GENERIC.id))
                && row["target"]["name"] == "jit_call_generic"
                && row["target"]["signature"] == "jsCall"
                && row["startOffset"]
                    .as_u64()
                    .is_some_and(|offset| start <= offset && offset < end)
        })
        .collect();
    assert_eq!(links.len(), 1, "own call enters one private JS trampoline");
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let link_end = usize::try_from(links[0]["endOffset"].as_u64().unwrap()).unwrap();
    #[cfg(target_arch = "aarch64")]
    {
        assert!(link_end + 4 <= end as usize);
        assert_eq!(
            u32::from_le_bytes(code[link_end..link_end + 4].try_into().unwrap()),
            0xd63f_0200,
            "own trampoline address is followed by BLR x16"
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        assert!(link_end + 3 <= end as usize);
        assert_eq!(
            &code[link_end..link_end + 3],
            &[0x41, 0xff, 0xd3],
            "own trampoline address is followed by CALL r11"
        );
    }
    let pc = u32::try_from(call["logicalPc"].as_u64().unwrap()).unwrap();
    let byte_pc = u32::try_from(call["bytePc"].as_u64().unwrap()).unwrap();
    let bytecode = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Bytecode)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert_eq!(bytecode.lines().next(), Some("; otter bytecode"));
    assert!(
        bytecode
            .lines()
            .nth(1)
            .unwrap()
            .starts_with(&format!("; function={} ", bundle.manifest().function_id()))
    );
    let canonical: Vec<_> = bytecode
        .lines()
        .skip(2)
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let logical_pc = words.next()?.parse::<u32>().ok()?;
            let encoded_pc = words.next()?.strip_prefix("byte=")?.parse::<u32>().ok()?;
            let opcode = words.next()?;
            (logical_pc == pc).then_some((encoded_pc, opcode))
        })
        .collect();
    assert_eq!(
        canonical,
        [(byte_pc, "Call")],
        "exact own canonical opcode and byte PC"
    );
    CollectingCall {
        function_id: bundle.manifest().function_id(),
        tier,
        code_object_id: bundle.manifest().code_object_id(),
    }
}

/// This safe diagnostic walks published native frames, excluding host frames.
/// No moving value or raw frame pointer escapes the callback observation.
fn published_collecting_caller(ctx: &mut RuntimeNativeCtx<'_>) -> PublishedCaller {
    // The active context may be the newer probe chunk. The safe stack walker
    // resolves each published frame in the shared code space independently.
    let frames_json = ctx
        .execution_context()
        .map(|context| ctx.capture_call_sites_json(context, 0, usize::MAX));
    PublishedCaller {
        frames_json,
        generations: ctx.interp_mut().jit_code_generation_snapshot(),
    }
}

fn assert_published_collecting_caller(
    observation: &PublishedCaller,
    expected: CollectingCall,
) -> serde_json::Value {
    let frames: serde_json::Value = serde_json::from_str(
        observation
            .frames_json
            .as_deref()
            .expect("published caller context"),
    )
    .expect("complete safe call-site JSON");
    let frames_array = frames.as_array().expect("complete call-site JSON array");
    assert_eq!(
        frames_array.len(),
        2,
        "own caller and script entry: {frames}"
    );
    assert_eq!(frames_array[0]["functionName"], "persistentMoveFields");
    assert!(
        frames_array[0]["scriptName"]
            .as_str()
            .unwrap()
            .ends_with("persistent-children-setup.js")
    );
    assert_eq!(frames_array[0]["lineNumber"], 8);
    assert_eq!(frames_array[0]["columnNumber"], 5);
    assert_eq!(
        frames_array[0]["sourceLine"],
        "    persistentCollect(left, right);"
    );
    assert_eq!(frames_array[1]["functionName"], "<main>");
    assert!(
        frames_array[1]["scriptName"]
            .as_str()
            .unwrap()
            .ends_with("persistent-children-probe.js")
    );
    let current: Vec<_> = observation
        .generations
        .iter()
        .filter(|entry| {
            entry.function_id == expected.function_id
                && entry.tier == expected.tier
                && entry.lifecycle == CodeLifetimeState::Installed
                && entry.linked
        })
        .collect();
    assert_eq!(
        current.len(),
        1,
        "sole caller generation during callback: {current:?}"
    );
    assert_eq!(current[0].code_object_id, expected.code_object_id);
    frames
}

#[test]
fn native_inline_and_suffix_children_survive_collecting_reentry() {
    let setup = r#"
const persistentChildren = {};
persistentChildren.p0 = {marker:17}; persistentChildren.p1 = 2;
persistentChildren.p2 = 3; persistentChildren.p3 = 4;
persistentChildren.p4 = {marker:29};
function persistentMoveFields(receiver) {
    const left = receiver.p0, right = receiver.p4;
    persistentCollect(left, right);
    return left.marker * 1000 + right.marker
      + (receiver.p0 === left ? 100000 : 0)
      + (receiver.p4 === right ? 200000 : 0);
}
for (let i = 0; i < 5000; i++) persistentMoveFields(persistentChildren);
for (let i = 5; i < 97; i++) persistentChildren['p' + i] = i;
for (let i = 0; i < 5000; i++) persistentMoveFields(persistentChildren);
"#;
    let selections = [JitSelection::Template, JitSelection::ProductionTiered];
    for selection in selections {
        let collect = Arc::new(AtomicBool::new(false));
        let collections = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(Mutex::new(CollectionFrames::default()));
        let trace = Arc::new(Mutex::new(Trace::default()));
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(RuntimeExtensionInstaller::new({
                let collect = collect.clone();
                let collections = collections.clone();
                let frames = frames.clone();
                move |realm| {
                    let collect = collect.clone();
                    let collections = collections.clone();
                    let frames = frames.clone();
                    moving_children::install(realm)?;
                    realm.install_native_global_call(
                        "persistentRenewChildren",
                        1,
                        RuntimeNativeCall::Dynamic(Arc::new(
                            |ctx: &mut RuntimeNativeCtx<'_>,
                             args: &[RuntimeValue],
                             _state: &[RuntimeValue]| {
                                ctx.scope(|mut scope| {
                                    let receiver = scope.argument(args, 0);
                                    let layout = scope.object_layout(&["marker"])?;
                                    let left_marker = scope.number(17.0);
                                    let right_marker = scope.number(29.0);
                                    let left = scope.object_with_layout(layout, &[left_marker])?;
                                    let right =
                                        scope.object_with_layout(layout, &[right_marker])?;
                                    // Publish only after both allocations: the first
                                    // child stays scoped, rather than becoming a
                                    // remembered old-parent edge before the second.
                                    scope.set(receiver, "p0", left)?;
                                    scope.set(receiver, "p4", right)?;
                                    let result = scope.undefined();
                                    Ok(scope.finish(result))
                                })
                            },
                        )),
                    )?;
                    realm.install_native_global_call(
                        "persistentCollect",
                        2,
                        RuntimeNativeCall::Dynamic(Arc::new(
                            move |ctx: &mut RuntimeNativeCtx<'_>,
                                  args: &[RuntimeValue],
                                  _state: &[RuntimeValue]| {
                                if collect.load(Ordering::Relaxed) {
                                    let before = published_collecting_caller(ctx);
                                    let children = moving_children::observe_and_collect(ctx, args)
                                        .map_err(|error| error.to_string());
                                    let after = published_collecting_caller(ctx);
                                    frames
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .observations
                                        .push(CollectionObservation {
                                            before,
                                            after,
                                            children,
                                        });
                                    collections.fetch_add(1, Ordering::Relaxed);
                                }
                                Ok(RuntimeValue::undefined())
                            },
                        )),
                    )?;
                    Ok(())
                }
            }))
            .build()
            .expect("persistent native field runtime");
        runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        let warm = runtime
            .run_script(
                SourceInput::from_javascript(setup),
                "persistent-children-setup.js",
            )
            .expect("warm both banks through suffix growth");
        let fid = *trace
            .lock()
            .unwrap()
            .names
            .iter()
            .find(|(_, name)| name.as_str() == "persistentMoveFields")
            .expect("actual field subject dispatch")
            .0;
        let tier = if selection == JitSelection::Template {
            NativeFrameKind::Baseline
        } else {
            NativeFrameKind::Optimizing
        };
        let current = runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|entry| {
                entry.function_id == fid
                    && entry.tier == tier
                    && entry.lifecycle == CodeLifetimeState::Installed
                    && entry.linked
            })
            .collect::<Vec<_>>();
        assert_eq!(
            current.len(),
            1,
            "own current native field entry: {current:?}"
        );
        let generation = &current[0];
        let artifacts = warm.jit_artifacts().expect("own native field artifacts");
        assert!(!artifacts.truncated());
        let bundle = artifacts
            .bundles()
            .iter()
            .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
            .expect("artifact of the exact current native field generation");
        assert_eq!(bundle.manifest().function_id(), fid);
        assert_eq!(bundle.manifest().function_name(), "persistentMoveFields");
        let expected_call = collecting_call_artifact(bundle, tier);
        assert!(
            !bundle
                .file(JitArtifactFileName::Code)
                .unwrap()
                .contents()
                .is_empty()
        );
        let report = warm.jit_debug_report().expect("field metadata");
        assert!(!report.truncated());
        let fields = report
            .events()
            .iter()
            .filter_map(|event| match event {
                JitDebugEvent::PropertyCacheIrSite {
                    function_id,
                    programs,
                    ..
                } if *function_id == fid => Some(programs),
                _ => None,
            })
            .flatten()
            .map(|program| program.field)
            .collect::<Vec<_>>();
        assert!(
            fields.contains(&FieldLocation::inline(0)),
            "actual native inline field"
        );
        assert!(
            fields.contains(&FieldLocation::overflow(0)),
            "actual native suffix field"
        );
        let mut oracle = Runtime::builder()
            .jit_selection(JitSelection::InterpreterOnly)
            .extension_installer(RuntimeExtensionInstaller::new(|realm| {
                realm.install_native_global_call(
                    "persistentCollect",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        |_ctx: &mut RuntimeNativeCtx<'_>,
                         _args: &[RuntimeValue],
                         _state: &[RuntimeValue]| {
                            Ok(RuntimeValue::undefined())
                        },
                    )),
                )?;
                Ok(())
            }))
            .build()
            .expect("independent interpreter");
        oracle
            .run_script(
                SourceInput::from_javascript(setup),
                "persistent-children-setup.js",
            )
            .expect("oracle setup");
        let source = "JSON.stringify([persistentMoveFields(persistentChildren),persistentChildren.p0.marker,persistentChildren.p4.marker,persistentChildren.p96])";
        let expected = oracle
            .run_script(
                SourceInput::from_javascript(source),
                "persistent-children-probe.js",
            )
            .expect("oracle probe");
        let before = runtime.execution_stats();
        collect.store(true, Ordering::Relaxed);
        trace.lock().unwrap().recording = true;
        let probe = runtime
            .run_script(
                SourceInput::from_javascript(format!(
                    "persistentRenewChildren(persistentChildren);{source}"
                )),
                "persistent-children-probe.js",
            )
            .expect("collecting native bank probe");
        trace.lock().unwrap().recording = false;
        let after = runtime.execution_stats();
        assert_eq!(probe.completion_string(), expected.completion_string());
        assert_eq!(probe.completion_string(), "[317029,17,29,96]");
        assert_eq!(
            collections.load(Ordering::Relaxed),
            1,
            "one real collecting callback"
        );
        assert!(after.gc_minor_cycles > before.gc_minor_cycles);
        assert!(
            after.gc_minor_slot_updates > before.gc_minor_slot_updates,
            "actual root relocation"
        );
        let observations = frames.lock().unwrap();
        assert_eq!(
            observations.observations.len(),
            1,
            "one own published native call observed before and after real collection"
        );
        let observation = &observations.observations[0];
        let before_stack = assert_published_collecting_caller(&observation.before, expected_call);
        let after_stack = assert_published_collecting_caller(&observation.after, expected_call);
        assert_eq!(
            before_stack, after_stack,
            "published caller survives moving GC"
        );
        let children = observation
            .children
            .as_ref()
            .expect("real scoped child collection");
        assert_eq!(
            children.len(),
            2,
            "exact inline and suffix child observations"
        );
        for (index, child) in children.iter().enumerate() {
            assert!(
                !children[..index]
                    .iter()
                    .any(|prior| prior.before == child.before),
                "distinct bank children have distinct live cells"
            );
            assert!(
                !children[..index]
                    .iter()
                    .any(|prior| prior.after == child.after),
                "distinct bank children remain distinct after collection"
            );
            assert_ne!(
                child.after, child.before,
                "the exact rooted bank child {index} must move"
            );
            assert_eq!(child.marker_before, [17.0, 29.0][index]);
            assert_eq!(child.marker_after, child.marker_before);
        }
        let trace = trace.lock().unwrap();
        assert_eq!(
            trace.count,
            trace.ticks.len(),
            "complete native-probe trace"
        );
        assert!(!trace.ticks.is_empty());
        assert!(
            !trace.ticks.contains(&fid),
            "subject stayed in its own native generation"
        );
        let current = runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|entry| {
                entry.function_id == fid
                    && entry.tier == tier
                    && entry.lifecycle == CodeLifetimeState::Installed
                    && entry.linked
            })
            .collect::<Vec<_>>();
        assert_eq!(
            current.len(),
            1,
            "sole current generation after collection: {current:?}"
        );
        let current = &current[0];
        assert_eq!(current.code_object_id, generation.code_object_id);
        assert_eq!(current.generated_deopts, generation.generated_deopts);
        if tier == NativeFrameKind::Baseline {
            assert_eq!(current.generated_entries, generation.generated_entries + 1);
        }
        assert!(
            !probe
                .jit_debug_report()
                .unwrap()
                .events()
                .iter()
                .any(|event| matches!(
                    event,
                    JitDebugEvent::CompilePrepared { .. }
                        | JitDebugEvent::Bail { .. }
                        | JitDebugEvent::EnteredGenerationDeopt { .. }
                        | JitDebugEvent::InlineDeoptFrame { .. }
                )),
            "collecting probe has no compilation or exit"
        );
    }
}

struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        trace
            .names
            .insert(event.function_id, event.function_name.into());
        if trace.recording {
            trace.count += 1;
            if trace.ticks.len() < 256 {
                trace.ticks.push(event.function_id);
            }
        }
    }
}

const SETUP: &str = r#"
const persistentReceiver = {};
persistentReceiver.p0 = 1; persistentReceiver.p1 = 2;
persistentReceiver.p2 = 3; persistentReceiver.p3 = 4;
persistentReceiver.p4 = 5;
function persistentFields(receiver) {
    if (arguments.length !== 1) throw new Error('arity');
    receiver.p0 = receiver.p0 + 1;
    receiver.p4 = receiver.p4 + 2;
    return receiver.p0 * 100 + receiver.p4;
}
for (let i = 0; i < 5000; i++) persistentFields(persistentReceiver);
// This replaces only the suffix allocation, twice, while p0 stays in-cell.
for (let i = 5; i < 33; i++) persistentReceiver['p' + i] = i;
for (let i = 0; i < 5000; i++) persistentFields(persistentReceiver);
"#;

#[test]
fn native_fields_keep_bank_identity_after_suffix_growth() {
    let selections = [JitSelection::Template, JitSelection::ProductionTiered];
    for selection in selections {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::events())
            .build()
            .unwrap();
        runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        let setup = runtime
            .run_script(
                SourceInput::from_javascript(SETUP),
                "persistent-fields-setup.js",
            )
            .unwrap();
        let fid = *trace
            .lock()
            .unwrap()
            .names
            .iter()
            .find(|(_, name)| name.as_str() == "persistentFields")
            .expect("actual source dispatch")
            .0;
        let tier = if selection == JitSelection::Template {
            NativeFrameKind::Baseline
        } else {
            NativeFrameKind::Optimizing
        };
        let current = runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|entry| {
                entry.function_id == fid
                    && entry.tier == tier
                    && entry.lifecycle == CodeLifetimeState::Installed
                    && entry.linked
            })
            .collect::<Vec<_>>();
        assert_eq!(
            current.len(),
            1,
            "own current {selection:?} entry: {current:?}"
        );
        let before = &current[0];
        let report = setup.jit_debug_report().unwrap();
        assert!(!report.truncated());
        let fields = report
            .events()
            .iter()
            .filter_map(|event| match event {
                JitDebugEvent::PropertyCacheIrSite {
                    function_id,
                    programs,
                    ..
                } if *function_id == fid => Some(programs),
                _ => None,
            })
            .flatten()
            .map(|program| program.field)
            .collect::<Vec<_>>();
        assert!(
            fields.contains(&FieldLocation::inline(0)),
            "native inline p0 proof"
        );
        assert!(
            fields.contains(&FieldLocation::overflow(0)),
            "native suffix p4 proof"
        );
        let mut oracle = Runtime::builder()
            .jit_selection(JitSelection::InterpreterOnly)
            .build()
            .unwrap();
        oracle
            .run_script(
                SourceInput::from_javascript(SETUP),
                "persistent-fields-setup.js",
            )
            .unwrap();
        let source = "JSON.stringify([persistentFields(persistentReceiver),persistentReceiver.p0,persistentReceiver.p4,persistentReceiver.p32])";
        let expected = oracle
            .run_script(
                SourceInput::from_javascript(source),
                "persistent-fields-probe.js",
            )
            .unwrap();
        trace.lock().unwrap().recording = true;
        let probe = runtime
            .run_script(
                SourceInput::from_javascript(source),
                "persistent-fields-probe.js",
            )
            .unwrap();
        trace.lock().unwrap().recording = false;
        assert_eq!(probe.completion_string(), expected.completion_string());
        assert_eq!(probe.completion_string(), "[1020207,10002,20007,32]");
        let trace = trace.lock().unwrap();
        assert_eq!(trace.count, trace.ticks.len(), "complete bounded trace");
        assert!(!trace.ticks.is_empty(), "probe main dispatch observed");
        assert!(!trace.ticks.contains(&fid), "subject executed natively");
        let after = runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|entry| {
                entry.function_id == fid
                    && entry.tier == tier
                    && entry.lifecycle == CodeLifetimeState::Installed
                    && entry.linked
            })
            .collect::<Vec<_>>();
        assert_eq!(
            after.len(),
            1,
            "sole current generation after bank probe: {after:?}"
        );
        let after = &after[0];
        assert_eq!(after.code_object_id, before.code_object_id);
        assert_eq!(after.generated_deopts, before.generated_deopts);
        if tier == NativeFrameKind::Baseline {
            assert_eq!(after.generated_entries, before.generated_entries + 1);
        }
        assert!(
            !probe
                .jit_debug_report()
                .unwrap()
                .events()
                .iter()
                .any(|event| matches!(
                    event,
                    JitDebugEvent::CompilePrepared { .. }
                        | JitDebugEvent::Bail { .. }
                        | JitDebugEvent::EnteredGenerationDeopt { .. }
                        | JitDebugEvent::InlineDeoptFrame { .. }
                )),
            "probe has no compile or exit"
        );
    }
}
