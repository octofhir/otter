//! Compiled property helpers reenter real getters with exact native root recipes.
//!
//! # Contents
//! - A megamorphic read remains in its own installed Template/Graph generation.
//! - A cold getter entered from its semantic helper performs real moving GC.
//! - Original source identities and current receiver/child aliases survive.
//!
//! # Invariants
//! The helper has no generated return anchor of its own. Its active stamped
//! recipe remains authoritative while the getter child runs. Observers retain
//! owned data only and never assert or unwind through the native ABI. A move is
//! checked from actual offsets; nursery placement is never inferred from policy.
//!
//! # See also
//! - `native_abi::call_trampoline` consumes the request's published association.
//! - `arm64::js_call` and `x86_64::js_call` publish genuine generated returns.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_runtime::{
    JitDebugRequest, JitDebugTier, JitSelection, Runtime, RuntimeExtensionInstaller,
    RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError, RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

const MODULE: &str = "semantic-reentry-anchor.js";
const SETUP: &str = r#"
var anchorPrototype = {value: 66};
var anchorInherited = Object.create(anchorPrototype);
var anchorReceivers = [
  {value: 1}, {a: 1, value: 2}, {b: 2, a: 1, value: 3},
  {c: 3, value: 4}, {d: 4, e: 5, value: 5}, anchorInherited
];
function anchorRead(receiver) {
  'use strict';
  for (var index = 0; index < 3; index++) {}
  return receiver.value;
}
var anchorArguments = [anchorReceivers[0]];
for (var warm = 0; warm < 5000; warm++) {
  anchorArguments[0] = anchorReceivers[warm % anchorReceivers.length];
  Reflect.apply(anchorRead, undefined, anchorArguments);
}
5000;
"#;
const PROBE: &str = r#"
anchorPrime();
globalThis.anchorChild = {marker: 731};
globalThis.anchorCurrentReceiver = Object.create(anchorPrototype);
globalThis.anchorCurrentAlias = anchorCurrentReceiver;
anchorCurrentReceiver.child = anchorChild;
var anchorGetterEffects = 0;
Object.defineProperty(anchorPrototype, 'value', {
  configurable: true,
  get: function anchorGetter() {
    anchorGetterEffects++;
    anchorCollect();
    return this.child;
  }
});
var anchorReturned = anchorRead(anchorCurrentReceiver);
JSON.stringify([anchorReturned === anchorChild, anchorReturned.marker,
  anchorCurrentReceiver === anchorCurrentAlias,
  anchorCurrentReceiver.child === anchorReturned, anchorGetterEffects]);
"#;

#[derive(Default)]
struct Counts {
    recording: bool,
    by_function: BTreeMap<u32, usize>,
}
struct Tracer(Arc<Mutex<Counts>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut counts = self.0.lock().unwrap();
        if counts.recording {
            *counts.by_function.entry(event.function_id).or_default() += 1;
        }
    }
}

#[derive(Clone, Debug)]
struct Roots {
    receiver: u32,
    alias: u32,
    child: u32,
    field_child: u32,
    marker: f64,
}
#[derive(Clone, Debug)]
struct Motion {
    before: Roots,
    after: Roots,
    minor_before: u64,
    minor_after: u64,
    source: String,
    active: Vec<u64>,
}
fn roots(ctx: &mut RuntimeNativeCtx<'_>) -> Result<Roots, RuntimeNativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope
            .global("anchorCurrentReceiver")
            .ok_or(RuntimeNativeError::InvalidOperand)?;
        let alias = scope
            .global("anchorCurrentAlias")
            .ok_or(RuntimeNativeError::InvalidOperand)?;
        let child = scope
            .global("anchorChild")
            .ok_or(RuntimeNativeError::InvalidOperand)?;
        let field_child = scope.get(receiver, "child")?;
        let marker = scope.get(child, "marker")?;
        let marker = scope.number_value(marker)?;
        let offset = |value: RuntimeValue| value.as_object().map(|object| object.offset());
        Ok(Roots {
            receiver: scope
                .scope(|nested| offset(nested.finish(receiver)))
                .ok_or(RuntimeNativeError::InvalidOperand)?,
            alias: scope
                .scope(|nested| offset(nested.finish(alias)))
                .ok_or(RuntimeNativeError::InvalidOperand)?,
            child: scope
                .scope(|nested| offset(nested.finish(child)))
                .ok_or(RuntimeNativeError::InvalidOperand)?,
            field_child: scope
                .scope(|nested| offset(nested.finish(field_child)))
                .ok_or(RuntimeNativeError::InvalidOperand)?,
            marker,
        })
    })
}
fn installer(records: Arc<Mutex<Vec<Result<Motion, String>>>>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        realm.install_native_global_call(
            "anchorPrime",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                |ctx: &mut RuntimeNativeCtx<'_>,
                 _args: &[RuntimeValue],
                 _state: &[RuntimeValue]| {
                    ctx.interp_mut().gc_heap_mut().set_gc_stress(0, false);
                    ctx.interp_mut().force_gc().map_err(|error| {
                        otter_vm::native_function::vm_to_native_error(
                            ctx.interp_mut(),
                            error.into(),
                            "anchor prime",
                        )
                    })?;
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        let records = records.clone();
        realm.install_native_global_call(
            "anchorCollect",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      _args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    let observed = (|| {
                        let before = roots(ctx)?;
                        let source = ctx
                            .execution_context()
                            .map(|context| ctx.capture_call_sites_json(context, 0, usize::MAX))
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let active = ctx
                            .interp_mut()
                            .jit_code_generation_snapshot()
                            .into_iter()
                            .filter(|generation| generation.active_count > 0)
                            .map(|generation| generation.code_object_id)
                            .collect();
                        let minor_before = ctx.interp_mut().gc_stats_snapshot().minor_gc_cycles;
                        ctx.interp_mut().force_gc().map_err(|error| {
                            otter_vm::native_function::vm_to_native_error(
                                ctx.interp_mut(),
                                error.into(),
                                "anchor collection",
                            )
                        })?;
                        let after = roots(ctx)?;
                        let minor_after = ctx.interp_mut().gc_stats_snapshot().minor_gc_cycles;
                        Ok::<_, RuntimeNativeError>(Motion {
                            before,
                            after,
                            minor_before,
                            minor_after,
                            source,
                            active,
                        })
                    })();
                    records
                        .lock()
                        .map_err(|_| RuntimeNativeError::InvalidOperand)?
                        .push(observed.as_ref().cloned().map_err(ToString::to_string));
                    observed.map(|_| RuntimeValue::undefined())
                },
            )),
        )
    })
}

#[test]
fn own_compiled_property_helper_keeps_zero_rust_origin_through_collecting_getter() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let records = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(installer(records.clone()))
            .build()
            .unwrap();
        let counts = Arc::new(Mutex::new(Counts::default()));
        runtime.set_tracer(Some(Box::new(Tracer(counts.clone()))));
        let warm = runtime
            .run_script(SourceInput::from_javascript(SETUP), MODULE)
            .unwrap();
        assert_eq!(warm.completion_string(), "5000");
        let own = if selection == JitSelection::InterpreterOnly {
            None
        } else {
            let tier = if selection == JitSelection::Template {
                JitDebugTier::Template
            } else {
                JitDebugTier::Optimizing
            };
            let bundle = warm
                .jit_artifacts()
                .unwrap()
                .bundles()
                .iter()
                .filter(|bundle| {
                    bundle.manifest().function_name() == "anchorRead"
                        && bundle.manifest().tier() == tier
                })
                .max_by_key(|bundle| bundle.manifest().code_object_id())
                .expect("own prepared native read artifact");
            let kind = if selection == JitSelection::Template {
                NativeFrameKind::Baseline
            } else {
                NativeFrameKind::Optimizing
            };
            Some(
                runtime
                    .jit_code_generation_snapshot()
                    .into_iter()
                    .find(|generation| {
                        generation.code_object_id == bundle.manifest().code_object_id()
                            && generation.tier == kind
                            && generation.current_entry
                            && generation.linked
                            && generation.lifecycle == CodeLifetimeState::Installed
                    })
                    .expect("own current generation"),
            )
        };
        counts.lock().unwrap().recording = true;
        let result = runtime
            .run_script(SourceInput::from_javascript(PROBE), "anchor-probe.js")
            .unwrap();
        counts.lock().unwrap().recording = false;
        assert_eq!(result.completion_string(), "[true,731,true,true,1]");
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "one collecting getter effect");
        let observed = records[0].as_ref().unwrap();
        assert!(observed.minor_after > observed.minor_before);
        assert_ne!(observed.before.receiver, observed.after.receiver);
        assert_ne!(observed.before.child, observed.after.child);
        for roots in [&observed.before, &observed.after] {
            assert_eq!(roots.receiver, roots.alias);
            assert_eq!(roots.child, roots.field_child);
            assert_eq!(roots.marker, 731.0);
        }
        let source: serde_json::Value = serde_json::from_str(&observed.source).unwrap();
        assert!(source.as_array().unwrap().iter().any(|frame|
            frame["functionName"] == "anchorRead" && frame["scriptName"] == MODULE));
        assert!(
            source
                .as_array()
                .unwrap()
                .iter()
                .any(|frame| frame["functionName"] == "anchorGetter"
                    && frame["scriptName"] == "anchor-probe.js")
        );
        if let Some(own) = own {
            assert!(
                observed.active.contains(&own.code_object_id),
                "actual native owner retained through Rust reentry"
            );
            assert_eq!(
                counts
                    .lock()
                    .unwrap()
                    .by_function
                    .get(&own.function_id)
                    .copied()
                    .unwrap_or(0),
                0,
                "no source replay of the native read"
            );
            let current = runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .find(|generation| generation.code_object_id == own.code_object_id)
                .unwrap();
            assert_eq!(current.generated_deopts, own.generated_deopts);
            assert!(current.linked && current.current_entry);
        }
    }
}
