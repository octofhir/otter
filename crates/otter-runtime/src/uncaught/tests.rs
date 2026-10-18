//! Actual Runtime reporter, source, FIFO and moving-exception proofs.
//!
//! # Contents
//! - Deliberate later turns report retained jobs in their original source.
//! - Collecting reporters preserve aliases or propagate exact Exit.
//! - Native-only timer admission restores realm and moved ambient async state.
//!
//! # Invariants
//! Observations are owned scalars/text recorded inside callbacks; assertions run
//! after the native ABI returns. All VM values retained through collection use
//! production scopes or persistent roots. Scheduler tokens remain owned data.
//!
//! # See also
//! - crate::uncaught and crate::timer_delivery own the tested extents.

use crate::Runtime;
use otter_vm::{NativeCallInfo, NativeCtx, NativeError};
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq)]
struct Seen {
    source: Option<String>,
    realm: u32,
    moved: bool,
    aliases: bool,
}

fn move_rooted(
    scope: &mut otter_vm::NativeScope<'_, '_>,
    value: otter_vm::Local<'_>,
) -> Result<bool, NativeError> {
    let root = scope.persistent_root_insert(value);
    let ambient = scope.async_context();
    let observed = scope.with_async_context(ambient, |ctx| {
        let before = ctx
            .persistent_root_get(root)
            .and_then(|value| value.as_object())
            .ok_or(NativeError::InvalidOperand)?
            .offset();
        let stats = ctx.interp_mut().gc_stats_snapshot();
        ctx.interp_mut().force_gc().map_err(NativeError::from)?;
        let after = ctx
            .persistent_root_get(root)
            .and_then(|value| value.as_object())
            .ok_or(NativeError::InvalidOperand)?
            .offset();
        Ok(before != after
            && ctx.interp_mut().gc_stats_snapshot().minor_gc_cycles > stats.minor_gc_cycles)
    });
    // Cleanup runs even if the real collector returns an error. The original
    // Local remains traced; no raw copy or finished scope survives collection.
    let retained = scope
        .take_persistent_root(root)
        .ok_or(NativeError::InvalidOperand)?;
    if !scope.strict_equals(retained, value) {
        return Err(NativeError::InvalidOperand);
    }
    observed
}

struct Scheduler;
impl otter_vm::TimerScheduler for Scheduler {
    fn admit(&self, _: bool) -> Result<otter_vm::TimerAdmission, String> {
        Ok(otter_vm::TimerAdmission::new(Box::new(())))
    }
    fn schedule(&self, _: otter_vm::TimerAdmission, _: u64, _: Option<u64>) -> Result<u64, String> {
        Ok(317)
    }
    fn cancel(&self, _: u64) -> bool {
        true
    }
    fn set_ref(&self, _: u64, _: bool) -> bool {
        true
    }
}

#[test]
fn native_only_timer_uses_origin_realm_and_restores_moved_ambient_async_before_exit() {
    let mut runtime = Runtime::builder().build().unwrap();
    runtime.interp.gc_heap_mut().set_gc_stress(0, true);
    runtime.install_timer_scheduler(Arc::new(Scheduler));
    let realm = runtime.interp.create_host_realm().unwrap();
    let realm_id = runtime
        .interp
        .with_host_realm(realm, |vm| Ok(vm.active_host_realm_id()))
        .unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let root = NativeCtx::with_host_context(
        &mut runtime.interp,
        NativeCallInfo::default_call(),
        None,
        |ctx| {
            ctx.scope(|mut scope| -> Result<_, NativeError> {
                let ambient = scope.object()?;
                scope.set_async_context(ambient);
                Ok(scope.persistent_root_insert(ambient))
            })
        },
    )
    .unwrap();
    runtime
        .interp
        .with_host_realm(realm, |vm| {
            Ok(NativeCtx::with_host_context(
                vm,
                NativeCallInfo::default_call(),
                None,
                |ctx| {
                    ctx.scope(|mut scope| -> Result<(), NativeError> {
                        let origin = scope.object()?;
                        let marker = scope.number(719.0);
                        scope.define(
                            origin,
                            "marker",
                            marker,
                            otter_vm::object::PropertyFlags::data_default(),
                        )?;
                        scope.set_async_context(origin);
                        let callback = scope.native_closure(
                            "native-only-timer",
                            0,
                            &[],
                            move |ctx, _, _| {
                                let source =
                                    ctx.execution_context().map(|c| c.module_name().to_owned());
                                let realm = ctx.interp_mut().active_host_realm_id();
                                ctx.scope(|mut scope| {
                                    let async_value = scope.async_context();
                                    let moved = move_rooted(&mut scope, async_value)?;
                                    let marker = scope.get(async_value, "marker")?;
                                    sink.lock().map_err(|_| NativeError::InvalidOperand)?.push(
                                        Seen {
                                            source,
                                            realm,
                                            moved,
                                            aliases: scope.number_value(marker).ok() == Some(719.0),
                                        },
                                    );
                                    Err(NativeError::Exit { code: 27 })
                                })
                            },
                        )?;
                        assert_eq!(scope.schedule_interval(callback, 1)?, 317);
                        Ok(())
                    })
                },
            ))
        })
        .unwrap()
        .unwrap();
    // Ambient values are explicitly chosen after scheduling, independently of
    // the entry's captured origin async context.
    runtime
        .interp
        .set_async_context(runtime.interp.persistent_root_get(root).unwrap());
    let result = runtime.fire_timer(317).expect("timer exact exit");
    assert!(matches!(
        result,
        crate::TimerFireOutcome::Fired { repeat: true }
    ));
    assert_eq!(runtime.take_pending_exit_code(), Some(27));
    assert_eq!(runtime.interp.active_host_realm_id(), 0);
    assert_eq!(
        runtime.interp.async_context(),
        runtime.interp.persistent_root_get(root).unwrap()
    );
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[Seen {
            source: None,
            realm: realm_id,
            moved: true,
            aliases: true
        }]
    );
    assert!(runtime.interp.cancel_timer(317));
    runtime.interp.persistent_root_remove(root);
}
