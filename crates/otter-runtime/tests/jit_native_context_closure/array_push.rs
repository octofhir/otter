//! Native array append before moving collection and across a capacity miss.
//!
//! # Contents
//! - Normal-policy admission of the actual ArrayPush operation.
//! - Spare-capacity append and full-buffer growth with unchanged generations.
//! - Default and additional source realms with complete dispatch traces.
//!
//! # Invariants
//! - The collecting callback runs after the append and cannot be replayed.
//! - The buffer and its slab are promoted before young values are appended.
//! - Native observers retain counters only; value construction uses handles.
//!
//! # See also
//! - The parent fixture owns native generation and interpreter trace assertions.
//! - `otter-vm/src/array/dense_push/tests.rs` isolates the remembered slab edge.

use super::*;
use std::sync::atomic::AtomicUsize;

const SOURCE: &str = r#"
function arrayPushSubject(array, child) {
  if (child === null) return arrayPushSubject(array, {marker: 0});
  const length = array.push(child);
  arrayPushCollect();
  return array[length - 1];
}
const arrayPushBuffer = [0, 1, 2, 3];
arrayPushBuffer.pop();
const arrayPushWarmChild = {marker: 731};
for (let warm = 0; warm < 20000; warm++) {
  arrayPushSubject(arrayPushBuffer, arrayPushWarmChild);
  arrayPushBuffer.pop();
}
"#;

fn case(extra: bool) {
    let collecting = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let extension = {
        let collecting = collecting.clone();
        let calls = calls.clone();
        RuntimeExtensionInstaller::new(move |realm| {
            let collecting = collecting.clone();
            let calls = calls.clone();
            realm.install_native_global_call(
                "arrayPushCollect",
                0,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |ctx: &mut RuntimeNativeCtx<'_>,
                          _: &[RuntimeValue],
                          _: &[RuntimeValue]| {
                        calls.fetch_add(1, Ordering::Relaxed);
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
            &"arrayPushSubject(arrayPushBuffer, arrayPushWarmChild); arrayPushBuffer.pop();\n"
                .repeat(16),
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
    for (name, marker) in [("spare", 817), ("growth", 919)] {
        let before = runtime.execution_stats();
        let before_calls = calls.load(Ordering::Relaxed);
        reset_trace(&trace);
        let result = run(
            &mut runtime,
            realm,
            &format!(
                "JSON.stringify(arrayPushSubject(arrayPushBuffer, {{marker: {marker}}}).marker);"
            ),
            &format!("native-array-push-{name}.js"),
        );
        assert_eq!(result.completion_string(), marker.to_string());
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
