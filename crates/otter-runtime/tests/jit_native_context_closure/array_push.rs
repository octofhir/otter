//! Native array append and shared Tagged storage through moving collection.
//!
//! # Contents
//! - Normal-policy admission of the actual ArrayPush operation.
//! - Spare-capacity, full-buffer and shared-literal growth in one generation.
//! - Actual owner/slab generations and the committed cold-protocol counter.
//! - Default and additional source realms with complete dispatch traces.
//!
//! # Invariants
//! - The collecting callback runs after the append and cannot be replayed.
//! - Array owners and shared literal storage are held before full collection.
//! - An ordinary scoped allocation refills the real LAB before each append.
//! - The default realm grows a young slab without the object-protocol fallback.
//! - Native observers retain scalar offsets/counters only; builders use handles.
//!
//! # See also
//! - The parent fixture owns native generation and interpreter trace assertions.
//! - `otter-vm/src/array/dense_push/tests.rs` isolates the remembered slab edge.

use super::*;
use otter_gc::page::SpaceKind;
use otter_runtime::ValueWindow;
use std::sync::atomic::AtomicUsize;

const SOURCE: &str = r#"
function arrayPushSubject(array, child, sibling) {
  if (child === null) return arrayPushSubject(array, {marker: 0}, sibling);
  arrayPushPrime(array, child, sibling);
  const length = array.push(child);
  arrayPushCollect(array, child, sibling);
  return array[length - 1];
}
function arrayPushTaggedFactory() {
  return [null, true, false, null, true, false, null, true];
}
function arrayPushTaggedPrefix(array) {
  return array[0] === null && array[1] === true && array[2] === false &&
    array[3] === null && array[4] === true && array[5] === false &&
    array[6] === null && array[7] === true;
}
const arrayPushTagged = arrayPushTaggedFactory();
const arrayPushTaggedSibling = arrayPushTaggedFactory();
const arrayPushBuffer = [0, 1, 2, 3];
arrayPushBuffer.pop();
const arrayPushWarmChild = {marker: 731};
for (let warm = 0; warm < 20000; warm++) {
  arrayPushSubject(arrayPushBuffer, arrayPushWarmChild, arrayPushBuffer);
  arrayPushBuffer.pop();
}
"#;

#[derive(Debug)]
struct AppendObservation {
    after_push: bool,
    owner: u32,
    child: u32,
    slab: u32,
    sibling_slab: u32,
    owner_space: Option<SpaceKind>,
    child_space: Option<SpaceKind>,
    slab_space: Option<SpaceKind>,
    slab_has_child: bool,
    machine_allocation_allowed: bool,
    reentrant: u64,
    minor: u64,
    after_prime_minor: u64,
}

fn observe(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: ValueWindow<'_>,
    after_push: bool,
) -> Result<AppendObservation, String> {
    let input = |index: usize| {
        args.get(index)
            .and_then(|value| value.as_raw_gc())
            .ok_or_else(|| format!("append observer argument {index} is not a GC value"))
    };
    let owner = input(0)?;
    let child = input(1)?;
    let sibling = input(2)?;
    let vm = ctx.interp_mut();
    let reentrant = vm.jit_runtime_stats().reentrant_stub_transitions;
    let minor = vm.gc_stats_snapshot().minor_gc_cycles;
    let heap = vm.gc_heap();
    // This cold snapshot traces actual edges without allocating in the JS heap.
    // Find the private slab by its registered type, without exposing its fields.
    let snapshot = heap.snapshot(&[owner, sibling]);
    let slab_of = |array| {
        snapshot.edges.iter().find_map(|&(from, to)| {
            if from != array {
                return None;
            }
            let body = &snapshot.objects[snapshot.index_of(to)?];
            heap.trace_table()
                .name(body.type_tag)?
                .ends_with("::ElementSlabBody")
                .then_some(to)
        })
    };
    let slab = slab_of(owner).ok_or_else(|| "array has no traced element slab".to_owned())?;
    let sibling_slab =
        slab_of(sibling).ok_or_else(|| "sibling has no traced element slab".to_owned())?;
    let mut owner_space = None;
    let mut child_space = None;
    let mut slab_space = None;
    heap.for_each_live_body(|space, body| {
        let raw = body.raw();
        if raw == owner {
            owner_space = Some(space);
        }
        if raw == child {
            child_space = Some(space);
        }
        if raw == slab {
            slab_space = Some(space);
        }
    });
    Ok(AppendObservation {
        after_push,
        owner: owner.0,
        child: child.0,
        slab: slab.0,
        sibling_slab: sibling_slab.0,
        owner_space,
        child_space,
        slab_space,
        slab_has_child: snapshot.edges.contains(&(slab, child)),
        machine_allocation_allowed: heap.machine_allocation_allowed(),
        reentrant,
        minor,
        after_prime_minor: minor,
    })
}

fn case(extra: bool) {
    let collecting = Arc::new(AtomicBool::new(false));
    let recording = Arc::new(AtomicBool::new(false));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let extension = {
        let collecting = collecting.clone();
        let recording = recording.clone();
        let observations = observations.clone();
        let calls = calls.clone();
        RuntimeExtensionInstaller::new(move |realm| {
            let prime_collecting = collecting.clone();
            let prime_recording = recording.clone();
            let prime_observations = observations.clone();
            realm.install_native_global_call(
                "arrayPushPrime",
                3,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |ctx: &mut RuntimeNativeCtx<'_>,
                          args: ValueWindow<'_>,
                          _: ValueWindow<'_>| {
                        if prime_collecting.load(Ordering::Relaxed) {
                            ctx.interp_mut().gc_heap_mut().set_gc_stress(0, true);
                            let mut observed = prime_recording
                                .load(Ordering::Relaxed)
                                .then(|| observe(ctx, args, false));
                            // Refill through the ordinary allocator, never a host-written LAB.
                            // Raw observer arguments are consumed before this allocation.
                            ctx.scope(|mut scope| {
                                let _ = scope.object()?;
                                Ok::<_, RuntimeNativeError>(())
                            })?;
                            if let Some(Ok(observed)) = &mut observed {
                                observed.after_prime_minor =
                                    ctx.interp_mut().gc_stats_snapshot().minor_gc_cycles;
                            }
                            if let Some(observed) = observed {
                                prime_observations.lock().unwrap().push(observed);
                            }
                        }
                        Ok(RuntimeValue::undefined())
                    },
                )),
            )?;
            let collecting = collecting.clone();
            let recording = recording.clone();
            let observations = observations.clone();
            let calls = calls.clone();
            realm.install_native_global_call(
                "arrayPushCollect",
                3,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |ctx: &mut RuntimeNativeCtx<'_>,
                          args: ValueWindow<'_>,
                          _: ValueWindow<'_>| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        if recording.load(Ordering::Relaxed) {
                            // Assert owned failures after returning across the native boundary.
                            let observed = observe(ctx, args, true);
                            observations.lock().unwrap().push(observed);
                        }
                        if collecting.load(Ordering::Relaxed) {
                            ctx.interp_mut().gc_heap_mut().set_gc_stress(1, true);
                            ctx.scope(|mut scope| {
                                for _ in 0..8 {
                                    let _ = scope.object()?;
                                }
                                Ok(RuntimeValue::undefined())
                            })
                        } else {
                            Ok(RuntimeValue::undefined())
                        }
                    },
                )),
            )
        })
    };
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(extension)
        .build()
        .unwrap();
    let realm = extra.then(|| runtime.create_realm().unwrap());
    let trace = Arc::new(Mutex::new(Trace::default()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let mut setup = run(&mut runtime, realm, SOURCE, "native-array-push-setup.js");
    let fid = trace
        .lock()
        .unwrap()
        .names
        .iter()
        .find_map(|(&fid, name)| (name == "arrayPushSubject").then_some(fid))
        .unwrap();
    let mut batch = setup.take_jit_artifacts().unwrap();
    drop(setup);
    runtime.force_gc().unwrap();
    collecting.store(true, Ordering::Relaxed);
    for attempt in 0..=128 {
        let mut warm = run(
            &mut runtime,
            realm,
            &"arrayPushSubject(arrayPushBuffer, arrayPushWarmChild, arrayPushBuffer); arrayPushBuffer.pop();\n".repeat(16),
            "native-array-push-admission.js",
        );
        let settled = !warm
            .jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .any(|event| {
                matches!(
                    event,
                    JitDebugEvent::Bail { .. }
                        | JitDebugEvent::EnteredGenerationDeopt { .. }
                        | JitDebugEvent::InlineDeoptFrame { .. }
                        | JitDebugEvent::CompilePrepared { .. }
                )
            });
        batch = batch.merged(warm.take_jit_artifacts().unwrap());
        if settled
            && runtime.jit_code_generation_snapshot().iter().any(|g| {
                g.function_id == fid
                    && g.tier == NativeFrameKind::Optimizing
                    && g.lifecycle == CodeLifetimeState::Installed
                    && g.linked
            })
        {
            break;
        }
        assert!(attempt < 128, "normal native admission must settle");
    }
    let generation = current(&runtime, fid);
    let ir = std::str::from_utf8(
        artifact(&batch, &generation)
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    if !extra {
        assert!(ir.contains("ArrayPush"), "{ir}");
    }
    recording.store(true, Ordering::Relaxed);
    let mut spare_reentrant = None;
    for (name, marker, array, sibling, length) in [
        ("spare", 817, "arrayPushBuffer", "arrayPushBuffer", 4),
        ("growth", 919, "arrayPushBuffer", "arrayPushBuffer", 5),
        (
            "shared",
            1021,
            "arrayPushTagged",
            "arrayPushTaggedSibling",
            9,
        ),
    ] {
        let before = runtime.execution_stats();
        let before_calls = calls.load(Ordering::Relaxed);
        reset_trace(&trace);
        let checks = if name == "shared" {
            ", arrayPushTaggedSibling.length === 8, arrayPushTaggedPrefix(arrayPushTagged), arrayPushTaggedPrefix(arrayPushTaggedSibling)"
        } else {
            ""
        };
        let result = run(
            &mut runtime,
            realm,
            &format!(
                "const arrayPush{name}Child = {{marker: {marker}}};\n\
                 const arrayPush{name}Result = arrayPushSubject({array}, arrayPush{name}Child, {sibling});\n\
                 JSON.stringify([arrayPush{name}Result === arrayPush{name}Child, arrayPush{name}Result.marker === {marker}, {array}.length === {length}{checks}]);"
            ),
            &format!("native-array-push-{name}.js"),
        );
        assert_eq!(
            result.completion_string(),
            if name == "shared" {
                "[true,true,true,true,true,true]"
            } else {
                "[true,true,true]"
            }
        );
        assert_native(
            &mut runtime,
            &trace,
            &result,
            std::slice::from_ref(&generation),
            fid,
        );
        assert_eq!(calls.load(Ordering::Relaxed), before_calls + 1);
        let after = runtime.execution_stats();
        assert!(after.gc_minor_cycles > before.gc_minor_cycles);
        assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
        let observed: Vec<_> = std::mem::take(&mut *observations.lock().unwrap())
            .into_iter()
            .map(|observed| observed.expect("append observer"))
            .collect();
        assert_eq!(observed.len(), 2, "{name}: {observed:?}");
        let prime = &observed[0];
        let appended = &observed[1];
        assert!(!prime.after_push && appended.after_push);
        assert_eq!(prime.owner_space, Some(SpaceKind::Old), "{name}");
        assert_eq!(prime.child_space, Some(SpaceKind::NewFrom), "{name}");
        assert_eq!(
            prime.minor, prime.after_prime_minor,
            "prime must not collect"
        );
        assert_eq!(prime.owner, appended.owner, "{name}");
        assert!(appended.slab_has_child, "{name}: {appended:?}");
        if !extra {
            assert!(prime.machine_allocation_allowed, "{name}");
            assert_eq!(
                prime.child, appended.child,
                "native append must not collect"
            );
            assert_eq!(
                prime.minor, appended.minor,
                "native append must not collect"
            );
            assert_eq!(appended.child_space, Some(SpaceKind::NewFrom), "{name}");
            let transitions = appended.reentrant - prime.reentrant;
            if name == "spare" {
                spare_reentrant = Some(transitions);
                assert_eq!(prime.slab, appended.slab);
                assert_eq!(appended.slab_space, Some(SpaceKind::Old));
            } else {
                assert_eq!(
                    Some(transitions),
                    spare_reentrant,
                    "{name}: object-protocol fallback adds a reentrant transition"
                );
                assert_ne!(prime.slab, appended.slab);
                assert_eq!(appended.slab_space, Some(SpaceKind::NewFrom));
            }
        }
        if name == "shared" {
            assert_eq!(prime.slab_space, Some(SpaceKind::Old));
            assert_eq!(
                prime.slab, prime.sibling_slab,
                "same literal site shares storage"
            );
            assert_eq!(prime.sibling_slab, appended.sibling_slab);
            assert_ne!(
                appended.slab, appended.sibling_slab,
                "append detaches its slab"
            );
        }
    }
}

#[test]
fn native_array_push_keeps_young_values_through_leaf_and_growth() {
    case(false);
}

#[test]
fn native_array_push_respects_additional_source_realm() {
    case(true);
}
