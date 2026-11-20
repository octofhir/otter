//! Compiler dispatch, cancellation and ownership regressions.
//!
//! # Contents
//! - Worker startup, queue growth, pending roots and terminal acknowledgements.
//!
//! # Invariants
//! - Spawn failure is injected per pool and worker observations are synchronized.
//!
//! # See also
//! - [`super::OptimizingCompileQueue`] owns the pending requests.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::code_space::CodeSpace;
use crate::jit_roots::CalleeIdentityCell;
use crate::{ExecutionContext, Value, source_registry::SourceRegistry};
use otter_resource::ResourceAccount;
use std::sync::{Weak, atomic::AtomicUsize, mpsc};

#[derive(Debug)]
struct CancellingHook {
    entered: mpsc::Sender<u32>,
    invocations: AtomicUsize,
    active_root: Weak<CalleeIdentityCell>,
    acknowledged: AtomicBool,
}

impl JitCompilerHook for CancellingHook {
    fn compile_function(
        &self,
        _request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        panic!("queue fixture compiles only optimizing jobs")
    }

    fn compile_optimized_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        self.invocations.fetch_add(1, Ordering::Relaxed);
        self.entered.send(request.snapshot.code_block.id).unwrap();
        while !request.cancellation.is_cancelled() {
            std::thread::yield_now();
        }
        assert!(
            self.active_root.upgrade().is_some(),
            "roots outlive active readers"
        );
        self.acknowledged.store(true, Ordering::Relaxed);
        Ok(JitCompileStatus::Cancelled)
    }
}

fn request(
    context: &ExecutionContext,
    code_object_id: u64,
) -> (JitCompileRequest, PendingCompile, Weak<CalleeIdentityCell>) {
    let snapshot = context
        .jit_compile_snapshot(context.function_base())
        .unwrap();
    let sources = CompilationSources::retain(context, &snapshot).unwrap();
    let cancellation = JitCompileCancellation::default();
    let root = Arc::new(CalleeIdentityCell::holding(Value::undefined()));
    let weak = Arc::downgrade(&root);
    let pending = PendingCompile {
        function_id: snapshot.code_block.id,
        osr_pc: None,
        tier: crate::jit_debug::JitDebugTier::Optimizing,
        function: snapshot.code_block.clone(),
        products: Arc::new(BakeProducts(Mutex::new(BakeOutput {
            roots: vec![CompilationRoot::CalleeIdentity(root)],
            sources: Some(sources),
            ..BakeOutput::default()
        }))),
        cancellation: cancellation.clone(),
        retry_code_reclamation: false,
        feedback_epoch: None,
        queued_ns: 0,
        perf_name: None,
    };
    let request = JitCompileRequest {
        cancellation,
        snapshot,
        debug: crate::JitDebugRequest::disabled(),
        artifact_identity: None,
        osr_pc: None,
        code_object_id,
    };
    (request, pending, weak)
}

#[test]
fn spawn_failure_runs_inline_only_when_no_worker_exists() {
    #[derive(Debug)]
    struct ReportThread(mpsc::Sender<(u64, std::thread::ThreadId)>);

    impl JitCompilerHook for ReportThread {
        fn compile_function(
            &self,
            request: JitCompileRequest,
        ) -> Result<JitCompileStatus, JitCompileError> {
            self.0
                .send((request.code_object_id, std::thread::current().id()))
                .unwrap();
            Ok(JitCompileStatus::Unavailable)
        }

        fn compile_optimized_function(
            &self,
            request: JitCompileRequest,
        ) -> Result<JitCompileStatus, JitCompileError> {
            self.compile_function(request)
        }
    }

    let space = Arc::new(CodeSpace::default());
    let account = ResourceAccount::default();
    let [first, second] = ["first.js", "second.js"].map(|name| {
        space
            .link_evictable_module(
                crate::test_support::minimal_bytecode_module(name),
                SourceRegistry::default(),
                &account,
            )
            .unwrap()
    });
    let mut queue = OptimizingCompileQueue::default();
    queue.set_threads(2);
    queue
        .pool
        .get_or_insert_with(CompilerPool::new)
        .fail_next_spawn = true;
    let (sender, reports) = mpsc::channel();
    let hook = Arc::new(ReportThread(sender));
    let submit = |queue: &mut OptimizingCompileQueue, context: &ExecutionContext, id| {
        let (request, pending, root) = request(context, id);
        let result = queue.submit(
            hook.clone(),
            CompileInput::Request(Box::new(request)),
            id,
            pending,
        );
        (result, root)
    };
    let caller = std::thread::current().id();
    let (result, root) = submit(&mut queue, &first, 1);
    let (owner, finished) = result.expect("first worker failed; job completes on caller");
    assert_eq!(finished.code_object_id, 1);
    assert!(matches!(finished.status, Ok(JitCompileStatus::Unavailable)));
    assert_eq!(reports.try_recv().unwrap(), (1, caller));
    assert_eq!(queue.pending_count(), 0);
    assert!(!queue.is_pending(first.function_base()));
    assert!(root.upgrade().is_some());
    drop(owner);
    assert!(root.upgrade().is_none());

    // Retry the same function. Its job remains pending until we collect
    // the result, so the next submission also attempts to grow the pool.
    assert!(submit(&mut queue, &first, 2).0.is_none());
    queue.pool.as_mut().unwrap().fail_next_spawn = true;
    assert!(submit(&mut queue, &second, 3).0.is_none());
    let (id, worker) = reports.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(id, 2);
    assert_ne!(worker, caller);
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(5)).unwrap(),
        (3, worker)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut completed = Vec::new();
    while queue.pending_count() > 0 {
        completed.extend(
            queue
                .take_finished()
                .into_iter()
                .map(|(_, job)| job.code_object_id),
        );
        assert!(Instant::now() < deadline, "worker did not publish results");
        std::thread::yield_now();
    }
    assert_eq!(completed, [2, 3]);
}

#[test]
fn cancellation_skips_queued_hooks_and_holds_active_owners_until_drop_ack() {
    let space = Arc::new(CodeSpace::default());
    let account = ResourceAccount::default();
    let contexts = ["active.js", "queued.js"].map(|name| {
        space
            .link_evictable_module(
                crate::test_support::minimal_bytecode_module(name),
                SourceRegistry::default(),
                &account,
            )
            .unwrap()
    });
    let (active, active_pending, active_root) = request(&contexts[0], 1);
    let (queued, queued_pending, queued_root) = request(&contexts[1], 2);
    let queued_id = queued.snapshot.code_block.id;
    let (entered, entries) = mpsc::channel();
    let hook = Arc::new(CancellingHook {
        entered,
        invocations: AtomicUsize::new(0),
        active_root: active_root.clone(),
        acknowledged: AtomicBool::new(false),
    });
    let mut queue = OptimizingCompileQueue::default();
    queue.set_threads(1);
    assert!(
        queue
            .submit(
                hook.clone(),
                CompileInput::Request(Box::new(active)),
                1,
                active_pending
            )
            .is_none()
    );
    assert_eq!(entries.recv().unwrap(), contexts[0].function_base());
    assert!(
        queue
            .submit(
                hook.clone(),
                CompileInput::Request(Box::new(queued)),
                2,
                queued_pending
            )
            .is_none()
    );
    // Both chunks are retained only by the pending owners, not requests'
    // CodeBlock Arcs. The worker cannot complete before cancellation.
    drop(contexts);
    assert!(space.eviction_candidates().is_empty());
    queue.invalidate(&[queued_id]);
    assert_eq!(queue.pending_count(), 2, "a queued ACK still owns its job");
    assert!(
        queue.is_pending(queued_id),
        "pending survives until terminal ACK consumption"
    );
    assert!(queued_root.upgrade().is_some());
    queue.mark_cancelled_for_reclamation_retry();
    assert!(queue.pending[&2].retry_code_reclamation);
    assert!(!queue.pending[&1].retry_code_reclamation);
    let finished = queue.take_finished();
    assert_eq!(
        queue.pending_count(),
        1,
        "the active compiler still owns its job"
    );
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].0.function_id, queued_id);
    assert_eq!(
        finished[0].1.pre_hook_exit,
        Some(crate::JitDebugPreHookExit::Cancelled)
    );
    assert_eq!(finished[0].1.duration, Duration::ZERO);
    assert!(matches!(
        finished[0].1.status,
        Ok(JitCompileStatus::Cancelled)
    ));
    assert!(active_root.upgrade().is_some());
    drop(finished);
    assert!(queued_root.upgrade().is_none());
    assert_eq!(space.eviction_candidates().len(), 1);
    // Drop must cancel the active token before joining, and preserve its
    // roots through the hook's final cancellation check/acknowledgement.
    drop(queue);
    assert!(active_root.upgrade().is_none());
    assert_eq!(space.eviction_candidates().len(), 2);
    assert_eq!(hook.invocations.load(Ordering::Relaxed), 1);
    assert!(hook.acknowledged.load(Ordering::Relaxed));
    assert!(
        entries.try_recv().is_err(),
        "cancelled FIFO job never enters its hook"
    );
}

#[test]
fn active_hook_cancellation_does_not_claim_a_pre_hook_refusal() {
    let space = Arc::new(CodeSpace::default());
    let context = space
        .link_evictable_module(
            crate::test_support::minimal_bytecode_module("active-cancel.js"),
            SourceRegistry::default(),
            &ResourceAccount::default(),
        )
        .unwrap();
    let (active, pending, root) = request(&context, 1);
    let token = pending.cancellation.clone();
    let (entered, entries) = mpsc::channel();
    let hook = Arc::new(CancellingHook {
        entered,
        invocations: AtomicUsize::new(0),
        active_root: root.clone(),
        acknowledged: AtomicBool::new(false),
    });
    let mut queue = OptimizingCompileQueue::default();
    queue.set_threads(1);
    assert!(
        queue
            .submit(
                hook.clone(),
                CompileInput::Request(Box::new(active)),
                1,
                pending
            )
            .is_none()
    );
    assert_eq!(
        entries.recv_timeout(Duration::from_secs(5)).unwrap(),
        context.function_base()
    );
    token.cancel();
    let deadline = Instant::now() + Duration::from_secs(5);
    let finished = loop {
        let rows = queue.take_finished();
        if !rows.is_empty() {
            break rows;
        }
        assert!(Instant::now() < deadline, "active cancellation did not ACK");
        std::thread::yield_now();
    };
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].1.pre_hook_exit, None);
    assert!(matches!(
        finished[0].1.status,
        Ok(JitCompileStatus::Cancelled)
    ));
    assert_eq!(hook.invocations.load(Ordering::Relaxed), 1);
    assert!(hook.acknowledged.load(Ordering::Relaxed));
    assert!(
        root.upgrade().is_some(),
        "active products outlive hook acknowledgement"
    );
    drop(finished);
    assert!(root.upgrade().is_none());
}
