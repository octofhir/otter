//! Native recursive splices and exact recovery of repeated source activations.
//!
//! # Contents
//! - Self and mutual recursion under the unchanged production tier policy.
//! - Own accepted Graph generations and isolated native execution witnesses.
//! - Innermost eager recovery, one coercion/throw, and real moving aliases.
//!
//! # Invariants
//! Each accepted splice owns its origin and frame state even when FIDs repeat.
//! Only current hosted source and the real collector execute. The observer
//! stores owned offsets/counters; it never retains a raw value across allocation.
//! Compilation budgets, entry thresholds, native ABI and counters are unchanged.
//!
//! # See also
//! - `inline_snapshot_budget` bounds cold preparation of owned descendants.
//! - `graph::builder` charges the existing encoded-bytecode and depth budgets.
//! - `interp::jit_calls::deopt` reconstructs ordered source activations.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use otter_runtime::{
    JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError,
    RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};

#[derive(Debug)]
struct Movement {
    phase: i32,
    offset: u32,
    minor: u64,
    active_codes: Vec<u64>,
}

#[derive(Default)]
struct Dispatch {
    recording: bool,
    by_function: BTreeMap<u32, usize>,
}

struct Tracer(Arc<Mutex<Dispatch>>);

impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut observed = self.0.lock().unwrap();
        if observed.recording {
            *observed.by_function.entry(event.function_id).or_default() += 1;
        }
    }
}

fn installer(observations: Arc<Mutex<Vec<Movement>>>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        realm.install_native_global_call(
            "recursiveStress",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _state: &[RuntimeValue]| {
                    let stride = args
                        .first()
                        .and_then(|value| value.as_number())
                        .ok_or_else(|| RuntimeNativeError::Error {
                            message: "missing recursive stress stride".into(),
                        })?
                        .as_f64() as u32;
                    ctx.interp_mut().gc_heap_mut().set_gc_stress(stride, true);
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        let observations = observations.clone();
        realm.install_native_global_call(
            "recursiveObserve",
            2,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    let offset = args
                        .first()
                        .copied()
                        .and_then(RuntimeValue::as_object)
                        .ok_or_else(|| RuntimeNativeError::Error {
                            message: "missing recursive alias".into(),
                        })?
                        .offset();
                    let phase = args
                        .get(1)
                        .and_then(|value| value.as_number())
                        .ok_or_else(|| RuntimeNativeError::Error {
                            message: "missing recursive phase".into(),
                        })?
                        .as_f64() as i32;
                    let vm = ctx.interp_mut();
                    let minor = vm.gc_stats_snapshot().minor_gc_cycles;
                    // Generations whose executable memory is still owned: an
                    // entry lease or published frame holds them, or no
                    // retirement epoch has released them yet.
                    let active_codes = vm
                        .jit_code_generation_snapshot()
                        .into_iter()
                        .filter(|generation| {
                            generation.active_count > 0
                                || generation.lifecycle != CodeLifetimeState::Retired
                        })
                        .map(|generation| generation.code_object_id)
                        .collect();
                    observations
                        .lock()
                        .map_err(|_| RuntimeNativeError::Error {
                            message: "recursive movement recorder poisoned".into(),
                        })?
                        .push(Movement {
                            phase,
                            offset,
                            minor,
                            active_codes,
                        });
                    Ok(RuntimeValue::undefined())
                },
            )),
        )
    })
}

fn setup(mutual: bool) -> String {
    let callee = if mutual { "recursiveB" } else { "recursiveA" };
    format!(
        r#"
recursiveStress(0);
globalThis.__recursiveNext = 1;
globalThis.__recursiveSentinel = {{ child: {{ tag: 17 }} }};
globalThis.__recursiveCoercions = 0;
globalThis.__recursiveThrow = false;
globalThis.__recursiveGarbage = {{ phase: 0 }};
function recursiveA(n, retained) {{
  if (n <= 0) return retained;
  const next = n === 2 ? globalThis.__recursiveNext : n - 1;
  return {callee}(next, retained);
}}
function recursiveB(n, retained) {{
  if (n <= 0) return retained;
  const next = n === 2 ? globalThis.__recursiveNext : n - 1;
  return recursiveA(next, retained);
}}
globalThis.__recursiveCoercible = {{
  valueOf() {{
    globalThis.__recursiveCoercions++;
    recursiveObserve(globalThis.__recursiveSentinel, 1);
    const garbage = {{ phase: 13 }};
    globalThis.__recursiveGarbage = garbage;
    recursiveObserve(globalThis.__recursiveSentinel, 2);
    if (globalThis.__recursiveThrow) throw globalThis.__recursiveSentinel;
    return 0;
  }}
}};
new Array(5000).fill(4).map(recursiveA).reduce((sum, value) => sum + value, 0);
"#
    )
}

const HIT: &str = r#"
(function recursiveHitProbe() {
globalThis.__recursiveSentinel = { child: { tag: 17 } };
return recursiveA(4, globalThis.__recursiveSentinel) === globalThis.__recursiveSentinel &&
globalThis.__recursiveSentinel.child.tag === 17;
})()
"#;

fn miss(throwing: bool) -> String {
    format!(
        r#"
(function recursiveMissProbe() {{
recursiveStress(0);
globalThis.__recursiveNext = globalThis.__recursiveCoercible;
globalThis.__recursiveCoercions = 0;
globalThis.__recursiveThrow = {throwing};
globalThis.__recursiveSentinel = {{ child: {{ tag: 17 }} }};
recursiveObserve(globalThis.__recursiveSentinel, 0);
recursiveStress(1);
let accepted = false;
try {{
  const returned = recursiveA(4, globalThis.__recursiveSentinel);
  accepted = !globalThis.__recursiveThrow && returned === globalThis.__recursiveSentinel;
}} catch (error) {{
  accepted = globalThis.__recursiveThrow && error === globalThis.__recursiveSentinel;
}}
recursiveStress(0);
return JSON.stringify([accepted, globalThis.__recursiveSentinel.child.tag,
  globalThis.__recursiveCoercions, globalThis.__recursiveGarbage.phase]);
}})()
"#
    )
}

fn eval(runtime: &mut Runtime, source: String, name: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), name)
        .unwrap()
        .completion_string()
        .to_owned()
}

#[test]
fn recursive_splices_keep_distinct_frames_and_move_recovered_aliases_once() {
    for mutual in [false, true] {
        for throwing in [false, true] {
            let observations = Arc::new(Mutex::new(Vec::new()));
            let install = installer(observations.clone());
            let mut oracle = Runtime::builder()
                .jit_selection(JitSelection::InterpreterOnly)
                .extension_installer(install.clone())
                .build()
                .unwrap();
            assert_eq!(
                eval(&mut oracle, setup(mutual), "recursive:oracle-setup"),
                "12497500"
            );
            assert_eq!(
                eval(&mut oracle, HIT.to_owned(), "recursive:oracle-hit"),
                "true"
            );
            assert_eq!(
                eval(&mut oracle, miss(throwing), "recursive:oracle-miss"),
                "[true,17,1,13]"
            );
            observations.lock().unwrap().clear();

            let mut runtime = Runtime::builder()
                .jit_selection(JitSelection::ProductionTiered)
                .jit_debug(JitDebugRequest::artifacts().with_events(true))
                .extension_installer(install)
                .build()
                .unwrap();
            let dispatch = Arc::new(Mutex::new(Dispatch::default()));
            runtime.set_tracer(Some(Box::new(Tracer(dispatch.clone()))));
            let warm = runtime
                .run_script(
                    SourceInput::from_javascript(setup(mutual)),
                    "recursive:native-setup",
                )
                .unwrap();
            assert_eq!(warm.completion_string(), "12497500");
            let report = warm.jit_debug_report().unwrap();
            assert!(!report.truncated());
            assert_eq!(report.dropped_events(), 0);
            let fids: BTreeSet<_> = report
                .events()
                .iter()
                .filter_map(|event| match event {
                    JitDebugEvent::CompilePrepared {
                        function_id,
                        function_name,
                        ..
                    } if function_name == "recursiveA"
                        || (mutual && function_name == "recursiveB") =>
                    {
                        Some(*function_id)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(fids.len(), if mutual { 2 } else { 1 });
            let root = report
                .events()
                .iter()
                .find_map(|event| match event {
                    JitDebugEvent::CompilePrepared {
                        function_id,
                        function_name,
                        ..
                    } if function_name == "recursiveA" => Some(*function_id),
                    _ => None,
                })
                .unwrap();
            let generations: Vec<_> = runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .filter(|generation| {
                    generation.function_id == root
                        && generation.tier == NativeFrameKind::Optimizing
                        && generation.lifecycle == CodeLifetimeState::Installed
                        && generation.linked
                        && generation.call_entry_offset.is_some()
                })
                .collect();
            assert_eq!(generations.len(), 1, "own current Graph recursive entry");
            let generation = &generations[0];
            let splices: Vec<_> = report
                .events()
                .iter()
                .filter_map(|event| match event {
                    JitDebugEvent::InlineLowered {
                        code_object_id,
                        tier: JitDebugTier::Optimizing,
                        parent_function_id,
                        callee_function_id,
                        depth,
                        cost,
                        ..
                    } if *code_object_id == generation.code_object_id => {
                        Some((*parent_function_id, *callee_function_id, *depth, *cost))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                splices
                    .iter()
                    .map(|splice| splice.2)
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([1, 2, 3])
            );
            assert!(
                splices
                    .iter()
                    .all(|&(parent, callee, _, cost)| fids.contains(&parent)
                        && fids.contains(&callee)
                        && cost <= 460)
            );
            assert!(splices.iter().map(|splice| splice.3).sum::<u32>() <= 920);
            assert_eq!(splices.iter().any(|splice| splice.0 == splice.1), !mutual);
            let artifacts = warm.jit_artifacts().unwrap();
            assert!(!artifacts.truncated());
            let own = artifacts
                .bundles()
                .iter()
                .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
                .unwrap();
            let deopt: serde_json::Value =
                serde_json::from_slice(own.file(JitArtifactFileName::Deopt).unwrap().contents())
                    .unwrap();
            assert!(
                deopt["frameStates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|exit| exit["frames"]
                        .as_array()
                        .is_some_and(|frames| frames.len() == 4)),
                "actual four-frame eager recipe"
            );

            let before = runtime.execution_stats();
            dispatch.lock().unwrap().recording = true;
            assert_eq!(
                eval(&mut runtime, HIT.to_owned(), "recursive:native-hit"),
                "true"
            );
            let after = runtime.execution_stats();
            // The interpreted probe enters the linked Graph entry through the
            // call trampoline, or at most once through the Rust entry; the
            // spliced recursion makes no further entry.
            assert!(
                after.jit_optimized_entries - before.jit_optimized_entries <= 1,
                "at most one Rust entry to the own current Graph body"
            );
            assert!(
                fids.iter().all(|fid| dispatch
                    .lock()
                    .unwrap()
                    .by_function
                    .get(fid)
                    .copied()
                    .unwrap_or(0)
                    == 0),
                "every subject body stays native"
            );
            let current = runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .find(|current| current.code_object_id == generation.code_object_id)
                .unwrap();
            assert_eq!(current.lifecycle, CodeLifetimeState::Installed);
            assert!(current.linked);
            assert_eq!(current.generated_deopts, generation.generated_deopts);

            let moved = runtime
                .run_script(
                    SourceInput::from_javascript(miss(throwing)),
                    "recursive:native-miss",
                )
                .unwrap();
            assert_eq!(moved.completion_string(), "[true,17,1,13]");
            let report = moved.jit_debug_report().unwrap();
            assert!(!report.truncated());
            assert_eq!(report.dropped_events(), 0);
            let recovered: Vec<_> = report
                .events()
                .iter()
                .filter_map(|event| match event {
                    JitDebugEvent::InlineDeoptFrame {
                        index,
                        total,
                        function_id,
                        ..
                    } => Some((*index, *total, *function_id)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                recovered.len(),
                3,
                "one innermost eager recovery of three descendants"
            );
            assert_eq!(
                recovered.iter().map(|frame| frame.0).collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
            assert!(
                recovered
                    .iter()
                    .all(|frame| frame.1 == 4 && fids.contains(&frame.2))
            );
            assert_eq!(
                recovered
                    .iter()
                    .map(|frame| frame.2)
                    .collect::<BTreeSet<_>>(),
                fids
            );
            let observed = observations.lock().unwrap();
            assert_eq!(
                observed.iter().map(|row| row.phase).collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
            assert_ne!(
                observed[0].offset, observed[2].offset,
                "retained alias actually relocates"
            );
            assert!(observed[2].minor > observed[0].minor && observed[2].minor > observed[1].minor);
            assert!(
                observed[1..]
                    .iter()
                    .all(|row| row.active_codes.contains(&generation.code_object_id)),
                "reconstructed callbacks run while the native owner's code is still owned"
            );
        }
    }
}
