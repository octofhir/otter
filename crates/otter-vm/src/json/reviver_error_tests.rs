//! Native JSON.parse reviver completion and source-context allocation proofs.
//!
//! # Contents
//! - Actual JSON.parse calls preserve source context and current results under GC.
//! - Pressure installed by one reviver exposes the next source-context OOM as
//!   the canonical catchable RangeError carrying the exact allocator cause.
//!
//! # Invariants
//! - Tests invoke the installed native entry through a real linked context.
//! - Callback observations are owned data; assertions execute outside callbacks.
//! - The collector and allocator cause every claimed relocation and refusal.
//!
//! # See also
//! - `super::native_parse` owns the coercion/source/result handle scope.
//! - `super::serialize::Interpreter::json_internalize_root` prepares source contexts.

use super::*;
use crate::{Interpreter, NativeCallInfo};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

#[derive(Default, Debug)]
struct Observation {
    visits: Vec<(String, Option<String>)>,
    pressure_cycles: Option<((u64, u64), (u64, u64))>,
    pressure_usage: Option<(u64, u64, u64)>,
}

fn callback_error(reason: &str) -> NativeError {
    NativeError::TypeError {
        name: "reviver-fixture",
        reason: reason.into(),
    }
}

fn snapshot(ctx: &NativeCtx<'_>, args: &[Value]) -> Result<(String, Option<String>), NativeError> {
    let key = args
        .first()
        .copied()
        .and_then(|value| value.as_string(ctx.heap()))
        .ok_or_else(|| callback_error("missing reviver key"))?
        .to_lossy_string(ctx.heap());
    let source = args
        .get(2)
        .copied()
        .and_then(Value::as_object)
        .and_then(|object| crate::object::get_own(object, ctx.heap(), "source"))
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|text| text.to_lossy_string(ctx.heap()));
    Ok((key, source))
}

#[test]
fn actual_native_reviver_keeps_source_and_result_through_repeated_moving_gc() {
    for stride in [1, 2, 4] {
        let mut vm = Interpreter::new().expect("reviver bootstrap");
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("json-reviver-gc"),
                crate::source_registry::SourceRegistry::default(),
            )
            .expect("actual source context");
        vm.gc_heap.set_gc_stress(stride, false);
        let observations = Arc::new(Mutex::new(Observation::default()));
        NativeCtx::with_host_context(
            &mut vm,
            NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| {
                    let json = scope.global("JSON").expect("JSON namespace");
                    let parse = scope.get(json, "parse").expect("installed native parse");
                    let input = scope
                        .string("{\"first\":1,\"second\":2}")
                        .expect("source text");
                    let callback = crate::native_function::NativeFunction::new(
                        scope.context().heap_mut(),
                        "reviver",
                        {
                            let observations = Arc::clone(&observations);
                            move |ctx, args, _| {
                                let visit = snapshot(ctx, args)?;
                                observations
                                    .lock()
                                    .map_err(|_| callback_error("observation lock"))?
                                    .visits
                                    .push(visit);
                                // The root invocation returns an object. Park it before
                                // this real allocation can move the current result.
                                ctx.scope(|mut scope| {
                                    let value = scope.value(
                                        args.get(1).copied().unwrap_or_else(Value::undefined),
                                    );
                                    let _ = scope.string("a real reviver allocation")?;
                                    Ok(scope.raw(value))
                                })
                            }
                        },
                    )
                    .expect("reviver body");
                    let callback = scope.value(Value::native_function(callback));
                    let before = scope.context().heap().gc_cycle_counts();
                    let result = scope
                        .call(parse, json, &[input, callback])
                        .expect("actual native parse call");
                    assert!(
                        scope.context().heap().gc_cycle_counts().0 > before.0,
                        "actual collector work during reviver traversal"
                    );
                    let first = scope.get(result, "first").expect("first result field");
                    let second = scope.get(result, "second").expect("second result field");
                    assert_eq!(scope.raw(first).as_f64(), Some(1.0));
                    assert_eq!(scope.raw(second).as_f64(), Some(2.0));
                    assert_eq!(
                        scope
                            .raw(input)
                            .as_string(scope.context().heap())
                            .unwrap()
                            .to_lossy_string(scope.context().heap()),
                        "{\"first\":1,\"second\":2}"
                    );
                    assert_eq!(
                        observations.lock().unwrap().visits,
                        vec![
                            ("first".into(), Some("1".into())),
                            ("second".into(), Some("2".into())),
                            ("".into(), None),
                        ]
                    );
                });
            },
        );
    }
}

#[test]
fn actual_native_reviver_source_context_refusal_keeps_allocator_cause() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("source-context bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("json-reviver-source-oom"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("actual source context");
    let observations = Arc::new(Mutex::new(Observation::default()));
    let reserved = Arc::new(AtomicU64::new(0));
    // The long second token must be reparsed for SameValue before context.source
    // is exposed. Leave small-key/context preparation headroom while making that
    // large allocation exceed the canonical effective cap headroom.
    let payload = "x".repeat(131072);
    let source = format!("{{\"first\":1,\"second\":\"{payload}\"}}");
    NativeCtx::with_host_context(
        &mut vm,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| {
                let json = scope.global("JSON").expect("namespace");
                let parse = scope
                    .get(json, "parse")
                    .expect("installed actual native entry");
                scope
                    .context()
                    .interp_mut()
                    .force_gc()
                    .expect("settle bootstrap and lookup garbage");
                let setup = scope.context().heap().gc_cycle_counts();
                let input = scope.string(&source).expect("long source");
                let child = scope.bare_object().expect("fresh rooted witness");
                let marker = scope.value(Value::number_i32(719));
                scope
                    .define(
                        child,
                        "marker",
                        marker,
                        crate::object::PropertyFlags::data_default(),
                    )
                    .expect("witness marker");
                let alias_value = scope.raw(child);
                let alias = scope.value(alias_value);
                let callback = crate::native_function::NativeFunction::new(
                    scope.context().heap_mut(),
                    "reviver",
                    {
                        let observations = Arc::clone(&observations);
                        let reserved = Arc::clone(&reserved);
                        move |ctx, args, _| {
                            let visit = snapshot(ctx, args)?;
                            let is_first = visit.0 == "first";
                            observations
                                .lock()
                                .map_err(|_| callback_error("observation lock"))?
                                .visits
                                .push(visit);
                            if is_first {
                                let before = ctx.heap().gc_cycle_counts();
                                let usage_before = ctx.heap().tracked_bytes();
                                let amount = cap
                                    .checked_sub(usage_before)
                                    .and_then(|available| available.checked_sub(8192))
                                    .ok_or_else(|| callback_error("invalid cap premise"))?;
                                ctx.heap_mut().reserve_bytes_no_collect(amount).map_err(
                                    |error| NativeError::OutOfMemory {
                                        name: "reviver-pressure",
                                        requested_bytes: error.requested_bytes(),
                                        heap_limit_bytes: error.heap_limit_bytes(),
                                    },
                                )?;
                                reserved.store(amount, Ordering::Relaxed);
                                let after = ctx.heap().gc_cycle_counts();
                                let usage_after = ctx.heap().tracked_bytes();
                                observations.lock().map_err(|_| callback_error("observation lock"))?
                                    .pressure_usage = Some((usage_before, usage_after, amount));
                                observations
                                    .lock()
                                    .map_err(|_| callback_error("observation lock"))?
                                    .pressure_cycles = Some((before, after));
                            }
                            Ok(args.get(1).copied().unwrap_or_else(Value::undefined))
                        }
                    },
                )
                .expect("pressure reviver");
                let callback = scope.value(Value::native_function(callback));
                assert_eq!(
                    scope.context().heap().gc_cycle_counts(),
                    setup,
                    "fresh input/witness setup without collection"
                );
                let before_child = scope.raw(child).as_object().unwrap().offset();
                let before = scope.context().heap().gc_cycle_counts();
                let result = scope.call(parse, json, &[input, callback]);
                let amount = reserved.swap(0, Ordering::Relaxed);
                scope.context().heap_mut().release_bytes(amount);
                assert!(
                    amount > 0,
                    "pressure was installed by the first actual reviver"
                );
                let observations = observations.lock().unwrap();
                assert_eq!(
                    observations.visits,
                    vec![("first".into(), Some("1".into()))]
                );
                let (pressure_before, pressure_after) =
                    observations.pressure_cycles.expect("pressure observations");
                assert_eq!(
                    pressure_after, pressure_before,
                    "pressure reservation itself cannot claim GC"
                );
                let (usage_before, usage_after, booked) = observations.pressure_usage.expect("effective pressure accounting");
                assert_eq!(booked, amount);
                assert_eq!(usage_after, usage_before + booked);
                assert_eq!(usage_after, cap - 8192);
                assert_eq!(cap - usage_after, 8192);
                assert!(cap - usage_after < payload.len() as u64,
                    "effective headroom already subtracts unused LAB tail; the large reparse cannot fit");
                assert!(
                    scope.context().heap().gc_cycle_counts().1 > before.1,
                    "actual source-context allocation refusal performs full collection"
                );
                // The refusal is an authored native OOM with diagnostic
                // headroom left, so it projects once into the canonical
                // catchable RangeError (`error_ops` invariants; see
                // `original_native_oom_with_headroom_has_the_canonical_catchable_range_class`).
                // That thrown value carries the exact allocator cause.
                match result.expect_err("source-context OOM cannot become a native TypeError") {
                    NativeError::Thrown { .. } => {}
                    other => {
                        panic!("expected the projected allocator RangeError, got {other:?}")
                    }
                }
                let thrown = scope
                    .context()
                    .interp_mut()
                    .take_pending_uncaught_throw()
                    .expect("projected RangeError is the pending throw");
                scope.context().clear_pending_error();
                let thrown = scope.value(thrown);
                let range_prototype = scope
                    .context()
                    .interp_mut()
                    .error_classes
                    .prototype(crate::ErrorKind::RangeError);
                assert!(crate::object::has_in_proto_chain(
                    scope.raw(thrown).as_object().expect("RangeError instance"),
                    scope.context().heap(),
                    range_prototype
                ));
                let message = scope.get(thrown, "message").expect("RangeError message");
                let message = scope.string_value(message).expect("message text");
                let requested_bytes = message
                    .strip_prefix("out of memory: requested ")
                    .and_then(|rest| rest.split(' ').next())
                    .and_then(|bytes| bytes.parse::<u64>().ok())
                    .expect("allocator cause in the RangeError message");
                assert!(
                    requested_bytes >= payload.len() as u64,
                    "large source-token allocation, rather than a small key/body allocation"
                );
                assert!(message.ends_with(&format!("heap limit {cap}")));
                assert_ne!(
                    scope.raw(child).as_object().unwrap().offset(),
                    before_child,
                    "actual rooted witness relocation"
                );
                assert_eq!(scope.raw(child), scope.raw(alias));
                let marker = scope
                    .get(child, "marker")
                    .expect("live child after source-context refusal");
                assert_eq!(scope.raw(marker), Value::number_i32(719));
                assert_eq!(
                    scope
                        .raw(input)
                        .as_string(scope.context().heap())
                        .unwrap()
                        .to_lossy_string(scope.context().heap()),
                    source
                );
                assert_eq!(scope.context().heap().stats().reserved_bytes, 0);
            });
        },
    );
}
