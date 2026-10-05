//! Host jobs distinguish deferred diagnostic text from a fresh synchronous throw.
//!
//! # Contents
//! - Actual completion jobs after a preceding turn left a moved throw root.
//! - A fresh IntoJs throw retained by the one pending owner through real GC.
//!
//! # Invariants
//! - Jobs own persistent promise ids and plain Rust data, never raw JS values.
//! - Observation resolves a separate persistent root after collecting settlement.
//! - Deferred text cannot claim the preceding turn's exception identity.
//!
//! # See also
//! - `super::settle_from_root` owns conversion and reaction drain.
//! - `crate::error_ops::throwable` owns synchronous exception consumption.

use super::*;
use crate::promise::PromiseState;
use std::sync::atomic::{AtomicU32, Ordering};

fn rooted_pending(vm: &mut Interpreter) -> (PersistentRootId, PersistentRootId) {
    let promise = crate::promise_dispatch::pending_runtime_rooted(vm, &[], &[])
        .expect("rooted pending promise");
    let value = Value::promise(promise);
    (
        vm.persistent_root_insert(value),
        vm.persistent_root_insert(value),
    )
}

fn leave_previous_throw(vm: &mut Interpreter) -> u32 {
    NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let value = cx.object().expect("preceding exception object");
            let raw = cx.escape(value);
            let old_offset = raw.as_object().unwrap().offset();
            let _ = cx.ctx().throw_value("preceding turn", raw);
            old_offset
        })
    })
}

#[test]
fn deferred_text_job_does_not_consume_a_preceding_turns_moved_throw() {
    let mut vm = Interpreter::new().expect("completion boundary bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, true);
    let (job_root, observation_root) = rooted_pending(&mut vm);
    let old_offset = leave_previous_throw(&mut vm);
    vm.pending_uncaught_frames = Some(vec![crate::StackFrameSnapshot {
        function_id: 719,
        function_name: "preceding turn".into(),
        module: "<preceding-host-turn>".into(),
        span: (31, 57),

        source_position: None,
    }]);
    let before = vm.gc_heap().gc_cycle_counts();
    vm.collect_minor_tracing_runtime_roots();
    assert!(vm.gc_heap().gc_cycle_counts().0 > before.0);
    assert_ne!(
        vm.pending_uncaught_throw
            .unwrap()
            .as_object()
            .unwrap()
            .offset(),
        old_offset
    );
    assert!(vm.pending_uncaught_frames.is_some());
    vm.gc_heap_mut().set_gc_stress(1, true);
    let before_job = vm.gc_heap().gc_cycle_counts();
    let realm_id = vm.active_host_realm_id();
    let job = HostCompletionJob::new(move |vm| {
        super::settle_from_root::<()>(
            vm,
            job_root,
            realm_id,
            None,
            Err(JsError::Native(crate::NativeError::Thrown {
                name: "worker",
                message: "worker-owned rejection".into(),
            })),
        )
    });
    job.run(&mut vm)
        .expect("text rejection settles in its own turn");
    assert!(vm.persistent_root_get(job_root).is_none());
    assert!(vm.pending_uncaught_throw.is_none());
    assert!(
        vm.pending_uncaught_frames.is_none(),
        "preceding throw provenance is cleared"
    );
    let after_job = vm.gc_heap().gc_cycle_counts();
    assert!(
        after_job.0 > before_job.0 || after_job.1 > before_job.1,
        "actual collection inside the text-only settlement"
    );
    let promise = vm
        .persistent_root_get(observation_root)
        .unwrap()
        .as_promise()
        .unwrap();
    let PromiseState::Rejected(reason) = promise.state(vm.gc_heap()) else {
        panic!("deferred text must reject the actual pending promise")
    };
    assert_eq!(
        reason
            .as_string(vm.gc_heap())
            .expect("owned worker text, not stale object")
            .to_lossy_string(vm.gc_heap()),
        "worker-owned rejection"
    );
    assert!(vm.persistent_root_remove(observation_root).is_some());
}

struct FreshThrow {
    before: Arc<AtomicU32>,
    after: Arc<AtomicU32>,
}

impl IntoJs for FreshThrow {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        let thrown = cx.object()?;
        let marker = cx.number(997.0);
        cx.define(
            thrown,
            "marker",
            marker,
            crate::object::PropertyFlags::data_default(),
        )?;
        cx.define(
            thrown,
            "self",
            thrown,
            crate::object::PropertyFlags::data_default(),
        )?;
        let raw = cx.escape(thrown);
        self.before
            .store(raw.as_object().unwrap().offset(), Ordering::SeqCst);
        let error = cx.ctx().throw_value("IntoJs callback", raw);
        // This is the same synchronous propagation extent. Pending throw and
        // handle aliases must both be rewritten before immediate materialization.
        let before = cx.heap().gc_cycle_counts();
        cx.ctx().interp_mut().collect_minor_tracing_runtime_roots();
        assert!(cx.heap().gc_cycle_counts().0 > before.0);
        let current = cx.escape(thrown);
        assert_eq!(cx.ctx().interp_mut().pending_uncaught_throw, Some(current));
        self.after
            .store(current.as_object().unwrap().offset(), Ordering::SeqCst);
        Err(JsError::from_native(error))
    }
}

#[test]
fn completion_conversion_consumes_only_its_fresh_synchronous_moved_throw() {
    let mut vm = Interpreter::new().expect("fresh throw completion bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, true);
    let (job_root, observation_root) = rooted_pending(&mut vm);
    leave_previous_throw(&mut vm);
    let before = Arc::new(AtomicU32::new(0));
    let after = Arc::new(AtomicU32::new(0));
    let value = FreshThrow {
        before: before.clone(),
        after: after.clone(),
    };
    let realm_id = vm.active_host_realm_id();
    let job = HostCompletionJob::new(move |vm| {
        super::settle_from_root(vm, job_root, realm_id, None, Ok(value))
    });
    job.run(&mut vm)
        .expect("fresh synchronous exception rejects promise");
    assert!(before.load(Ordering::SeqCst) != 0);
    assert!(after.load(Ordering::SeqCst) != 0);
    assert_ne!(before.load(Ordering::SeqCst), after.load(Ordering::SeqCst));
    assert!(vm.pending_uncaught_throw.is_none());
    assert!(vm.pending_uncaught_frames.is_none());
    assert!(vm.persistent_root_get(job_root).is_none());
    let promise = vm
        .persistent_root_get(observation_root)
        .unwrap()
        .as_promise()
        .unwrap();
    let PromiseState::Rejected(reason) = promise.state(vm.gc_heap()) else {
        panic!("fresh throw must reject the actual pending promise")
    };
    assert_eq!(
        reason.as_object().unwrap().offset(),
        after.load(Ordering::SeqCst)
    );
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let reason = cx.park(reason);
            let marker = cx.get(reason, "marker").unwrap();
            assert_eq!(cx.as_f64(marker), Some(997.0));
            let alias = cx.get(reason, "self").unwrap();
            assert_eq!(cx.escape(alias), cx.escape(reason));
        })
    });
    assert!(vm.persistent_root_remove(observation_root).is_some());
}

struct CountConversion(Arc<std::sync::atomic::AtomicUsize>);
impl IntoJs for CountConversion {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(cx.undefined())
    }
}

#[test]
fn completion_source_realm_and_disposal_share_the_current_root_owner() {
    let mut vm = Interpreter::new().expect("realm completion bootstrap");
    let source = vm.create_host_realm().expect("source realm");
    let other = vm.create_host_realm().expect("other realm");
    let (job_root, observation_root, prototype_root) = vm
        .with_host_realm(source, |vm| {
            let (job, observed) = rooted_pending(vm);
            let prototype = vm.error_classes.prototype(crate::ErrorKind::SyntaxError);
            Ok((
                job,
                observed,
                vm.persistent_root_insert(Value::object(prototype)),
            ))
        })
        .unwrap();
    vm.gc_heap_mut().set_gc_stress(1, true);
    let job = HostCompletionJob::new(move |vm| {
        super::settle_from_root::<()>(
            vm,
            job_root,
            source.0,
            None,
            Err(JsError::Native(crate::NativeError::SyntaxError {
                name: "source realm",
                reason: "exact rejection".into(),
            })),
        )
    });
    vm.with_host_realm(other, |vm| {
        assert_eq!(vm.active_host_realm_id(), other.0);
        let before = vm.gc_heap().gc_cycle_counts();
        let result = job.run(vm);
        assert_eq!(
            vm.active_host_realm_id(),
            other.0,
            "ambient realm restored after source operation"
        );
        let after = vm.gc_heap().gc_cycle_counts();
        assert!(after.0 > before.0 || after.1 > before.1);
        Ok(result)
    })
    .unwrap()
    .expect("source realm completion");
    let promise = vm
        .persistent_root_get(observation_root)
        .unwrap()
        .as_promise()
        .unwrap();
    let PromiseState::Rejected(reason) = promise.state(vm.gc_heap()) else {
        panic!("source rejection")
    };
    let original = vm
        .persistent_root_get(prototype_root)
        .unwrap()
        .as_object()
        .unwrap();
    assert!(crate::object::has_in_proto_chain(
        reason.as_object().unwrap(),
        vm.gc_heap(),
        original
    ));
    assert!(vm.persistent_root_get(job_root).is_none());
    vm.persistent_root_remove(observation_root);
    vm.persistent_root_remove(prototype_root);

    let disposed = vm.create_host_realm().expect("disposable origin");
    let (job_root, observation_root) = vm
        .with_host_realm(disposed, |vm| Ok(rooted_pending(vm)))
        .unwrap();
    assert!(vm.dispose_host_realm(disposed));
    let conversions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let value = CountConversion(conversions.clone());
    let job = HostCompletionJob::new(move |vm| {
        super::settle_from_root(vm, job_root, disposed.0, None, Ok(value))
    });
    job.run(&mut vm)
        .expect("disposed origin releases its job root");
    assert_eq!(
        conversions.load(Ordering::SeqCst),
        0,
        "no conversion in a foreign realm"
    );
    assert!(vm.persistent_root_get(job_root).is_none());
    let promise = vm
        .persistent_root_get(observation_root)
        .unwrap()
        .as_promise()
        .unwrap();
    assert_eq!(promise.state(vm.gc_heap()), PromiseState::Pending);
    assert!(vm.persistent_root_remove(observation_root).is_some());
}
