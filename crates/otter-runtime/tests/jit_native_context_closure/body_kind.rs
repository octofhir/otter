//! Native body-kind proofs across collecting effects and a live large length.
//!
//! # Contents
//! - Normal-policy literal/field/length code and exact retained generation proof.
//! - Moving roots through a scoped native callback with unchanged receiver kinds.
//! - Length overflow resumes after the callback without replaying it.
//!
//! # Invariants
//! - Warmup settles allocation-site decisions before freezing the native body.
//! - Every native value mutation/build uses handles; host controls are owned.
//! - Only the isolated subject executes during measured entry probes.
//!
//! # See also
//! - The parent fixture owns complete interpreter traces and generation lookup.
//! - `otter-difftest/corpus/physical_body_proofs.js` covers receiver replacements.

use super::*;
use std::sync::atomic::AtomicUsize;

const SOURCE: &str = r#"
function bodyKindSubject(child, value) {
  if (value < 0) return bodyKindSubject.apply(null, arguments);
  const array = [child, value];
  const object = {value};
  const before = array.length;
  bodyKindEffect(array, object);
  return [before, array.length, object.value, array[0] === child, child.tag];
}
const bodyKindWarmChild = {tag: 5};
for (let warm = 0; warm < 20000; warm++) bodyKindSubject(bodyKindWarmChild, warm & 7);
"#;
const PROBE: &str = "JSON.stringify(bodyKindSubject({tag: 5}, 3));";

fn extension(phase: Arc<AtomicUsize>, calls: Arc<AtomicUsize>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        let phase = phase.clone();
        let calls = calls.clone();
        realm.install_native_global_call(
            "bodyKindEffect",
            2,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    let phase = phase.load(Ordering::Relaxed);
                    if phase != 0 {
                        ctx.interp_mut().gc_heap_mut().set_gc_stress(1, true);
                    }
                    ctx.scope(|mut scope| {
                        let array = scope.value(args[0]);
                        let object = scope.value(args[1]);
                        let length = scope.number(if phase == 2 { 2147483648.0 } else { 3.0 });
                        if phase == 2 {
                            let key = scope.atom("length");
                            scope.set_atom(array, &key, length)?;
                        } else {
                            scope.set(array, "length", length)?;
                        }
                        let value = scope.number(7.0);
                        scope.set(object, "value", value)?;
                        if phase != 0 {
                            for _ in 0..8 {
                                let _ = scope.string("body-kind moving-root allocation")?;
                            }
                        }
                        Ok(RuntimeValue::undefined())
                    })
                },
            )),
        )
    })
}

fn case(extra: bool) {
    let phase = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(extension(phase.clone(), calls.clone()))
        .build()
        .unwrap();
    let realm = extra.then(|| runtime.create_realm().unwrap());
    let trace = Arc::new(Mutex::new(Trace::default()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let mut setup = run(&mut runtime, realm, SOURCE, "native-body-kind-setup.js");
    let fid = trace
        .lock()
        .unwrap()
        .names
        .iter()
        .find_map(|(&fid, name)| (name == "bodyKindSubject").then_some(fid))
        .unwrap();
    let mut batch = setup.take_jit_artifacts().unwrap();
    drop(setup);
    runtime.force_gc().unwrap();
    phase.store(1, Ordering::Relaxed);
    for attempt in 0..=128 {
        let mut warm = run(
            &mut runtime,
            realm,
            &"bodyKindSubject(bodyKindWarmChild, 3);\n".repeat(16),
            "native-body-kind-admission.js",
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
    let bundle = artifact(&batch, &generation);
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert!(
        ir.matches("LoadArrayLength { array_proved: true }").count() >= 2,
        "{ir}"
    );
    assert!(ir.contains("object_proved: true"), "{ir}");
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let lengths: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| {
            r["kind"] == "instruction"
                && r["functionId"] == fid
                && r["operation"]
                    .as_str()
                    .is_some_and(|s| s.contains("LoadArrayLength"))
        })
        .collect();
    assert!(lengths.len() >= 2);
    for region in lengths {
        assert!(region["startOffset"].as_u64().unwrap() < region["endOffset"].as_u64().unwrap());
    }
    let before = runtime.execution_stats();
    let before_calls = calls.load(Ordering::Relaxed);
    reset_trace(&trace);
    let result = run(&mut runtime, realm, PROBE, "native-body-kind-probe.js");
    assert_eq!(result.completion_string(), "[2,3,7,true,5]");
    assert_native(
        &mut runtime,
        &trace,
        &result,
        std::slice::from_ref(&generation),
        fid,
    );
    let after = runtime.execution_stats();
    assert!(after.gc_minor_cycles > before.gc_minor_cycles);
    assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
    assert_eq!(calls.load(Ordering::Relaxed), before_calls + 1);

    phase.store(2, Ordering::Relaxed);
    let before_calls = calls.load(Ordering::Relaxed);
    let result = run(
        &mut runtime,
        realm,
        PROBE,
        "native-body-kind-large-length.js",
    );
    assert_eq!(result.completion_string(), "[2,2147483648,7,true,5]");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        before_calls + 1,
        "length exit must not replay callback"
    );
    assert!(
        result
            .jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(
                event,
                JitDebugEvent::Bail { .. }
                    | JitDebugEvent::EnteredGenerationDeopt { .. }
                    | JitDebugEvent::InlineDeoptFrame { .. }
            )),
        "large length must reach the retained range guard"
    );
}

#[test]
fn native_body_kind_proofs_survive_moving_gc_and_large_length() {
    case(false);
}

#[test]
fn native_body_kind_proofs_keep_their_additional_source_realm() {
    case(true);
}
