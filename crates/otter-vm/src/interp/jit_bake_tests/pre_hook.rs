//! Pre-hook bake refusal under real worker, shared-state and moving-GC boundaries.
//!
//! # Contents
//! - A test-only per-job rendezvous after acquiring the heap read right.
//! - GC-yield acknowledgement, pending-root rewriting and explicit fresh attempts.
//! - Shared-table contention and cancellation/source refusal classification.
//!
//! # Invariants
//! No scheduler sleep, tier forcing or retry implementation determines success.
//! The mutator performs a real allocation-driven pause while the worker holds
//! the read right; every wait is bounded and panic unwinding releases that right.
//! A pending root remains collector-owned through terminal acknowledgement.
//!
//! # See also
//! - `jit_compile_queue::tests` covers queued cancellation and source leases.

#[path = "pre_hook/completed_body.rs"]
mod completed_body;
pub(crate) use completed_body::{BeforeShapeCensus, PublishReady};

#[path = "pre_hook/pending_publication.rs"]
mod pending_publication;

use super::*;
use crate::jit_compile_queue::{BakeProducts, CompileInput, FinishedCompile, PendingCompile};
use crate::jit_roots::{CalleeIdentityCell, CompilationRoot};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

pub(crate) struct Checkpoint {
    acquired: mpsc::Sender<()>,
    wait_for_yield: bool,
}

impl Checkpoint {
    pub(crate) fn arrive(self, guard: &otter_gc::read_right::HeapReadGuard<'_>) {
        self.acquired.send(()).expect("mutator awaits reader");
        if self.wait_for_yield {
            let deadline = Instant::now() + WAIT;
            while !guard.yield_requested() {
                assert!(
                    Instant::now() < deadline,
                    "mutator did not request the pause"
                );
                std::thread::yield_now();
            }
        }
    }
}

pub(crate) struct BeforeHook {
    ready: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

impl BeforeHook {
    pub(crate) fn arrive(self) {
        self.ready.send(()).expect("mutator awaits completed bake");
        self.resume
            .recv_timeout(WAIT)
            .expect("mutator releases completed bake");
    }
}

#[derive(Debug, Default)]
struct RecordingHook {
    entries: AtomicUsize,
    cancel_on_entry: bool,
}

impl jit::JitCompilerHook for RecordingHook {
    fn compile_function(
        &self,
        request: jit::JitCompileRequest,
    ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
        assert!(!request.cancellation.is_cancelled());
        self.entries.fetch_add(1, Ordering::Relaxed);
        if self.cancel_on_entry {
            request.cancellation.cancel();
        }
        Ok(jit::JitCompileStatus::Unavailable)
    }

    fn compile_optimized_function(
        &self,
        request: jit::JitCompileRequest,
    ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
        self.compile_function(request)
    }
}

fn fixture() -> (Interpreter, ExecutionContext) {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    vm.set_jit_compiler_threads(1);
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("pre-hook-worker.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("linked source");
    (vm, context)
}

fn job(
    vm: &Interpreter,
    context: &ExecutionContext,
    tier: JitDebugTier,
    id: u64,
    root: Option<Arc<CalleeIdentityCell>>,
    checkpoint: Option<Checkpoint>,
) -> (BakeJob, PendingCompile) {
    let env = crate::interp::jit_bake_env::BakeEnv::capture(vm);
    let mut roots: Vec<_> = env.held_roots().collect();
    if let Some(root) = root {
        roots.push(CompilationRoot::CalleeIdentity(root));
    }
    let products = Arc::new(BakeProducts::holding(roots));
    let cancellation = JitCompileCancellation::default();
    let fid = context.function_base();
    let pending = PendingCompile {
        function_id: fid,
        osr_pc: None,
        tier,
        function: context.code_block_arc(fid).unwrap(),
        products: products.clone(),
        cancellation: cancellation.clone(),
        retry_code_reclamation: false,
        feedback_epoch: None,
        queued_ns: 0,
        perf_name: None,
    };
    (
        BakeJob {
            checkpoint,
            before_hook: None,
            publish_ready: None,
            before_shape_census: None,
            env,
            context: context.clone(),
            function_id: fid,
            osr_pc: None,
            tier,
            dependency_flag: vm.jit_code_registry.dependency_flag(),
            debug: JitDebugRequest::disabled(),
            artifact_identity: None,
            code_object_id: id,
            cancellation,
            products,
        },
        pending,
    )
}

fn finished(
    queue: &mut crate::jit_compile_queue::OptimizingCompileQueue,
) -> (PendingCompile, FinishedCompile) {
    let deadline = Instant::now() + WAIT;
    loop {
        let mut rows = queue.take_finished();
        if !rows.is_empty() {
            assert_eq!(rows.len(), 1, "one job has one terminal ACK");
            return rows.pop().unwrap();
        }
        assert!(Instant::now() < deadline, "worker did not acknowledge");
        std::thread::yield_now();
    }
}

#[test]
fn moving_pause_refusal_keeps_pending_roots_and_requires_a_fresh_request() {
    for tier in [JitDebugTier::Template, JitDebugTier::Optimizing] {
        let (mut vm, context) = fixture();
        vm.set_jit_debug_request(JitDebugRequest::events());
        let yield_point = JitDebugBakeYieldPoint::RootStart;
        let yield_point_json = "rootStart";
        let hook = Arc::new(RecordingHook::default());
        for id in 1..=3 {
            // Only the pending CompilationRoot registers this cell with GC.
            // Keeping an Arc here permits observation, but does not trace it.
            let root = vm.with_handle_scope(|vm, scope| {
                let marker = vm.scoped_value(scope, Value::number_i32(id as i32 + 731));
                let done = vm.scoped_value(scope, Value::boolean(true));
                let object = vm
                    .scoped_iterator_result(scope, marker, done)
                    .expect("young marked root");
                Arc::new(CalleeIdentityCell::holding(vm.escape_scoped(object)))
            });
            let before = root.held().as_object().unwrap().offset();
            let cycles = vm.gc_heap.gc_stats().minor_gc_cycles;
            let (acquired, reader) = mpsc::channel();
            let (bake, pending) = job(
                &vm,
                &context,
                tier,
                id,
                Some(root.clone()),
                Some(Checkpoint {
                    acquired,
                    wait_for_yield: true,
                }),
            );
            assert!(
                vm.jit_compile_queue
                    .submit(
                        hook.clone(),
                        CompileInput::Bake(Box::new(bake)),
                        id,
                        pending
                    )
                    .is_none()
            );
            reader
                .recv_timeout(WAIT)
                .expect("worker owns heap read right");
            vm.gc_heap.set_gc_stress(1, false);
            vm.with_handle_scope(|vm, scope| {
                vm.scoped_object_bare(scope)
                    .expect("allocation requests the minor pause");
            });
            assert_eq!(vm.gc_heap.gc_stats().minor_gc_cycles, cycles + 1);
            assert_ne!(
                root.held().as_object().unwrap().offset(),
                before,
                "the pending root actually moved"
            );
            let moved = root.held().as_object().unwrap();
            assert_eq!(
                crate::object::get(moved, &vm.gc_heap, "value"),
                Some(Value::number_i32(id as i32 + 731))
            );
            assert_eq!(
                crate::object::get(moved, &vm.gc_heap, "done"),
                Some(Value::boolean(true))
            );
            assert_eq!(
                vm.jit_compile_queue.pending_count(),
                1,
                "terminal result still owns its pending root"
            );
            let weak = Arc::downgrade(&root);
            drop(root);
            if id == 3 {
                let deadline = Instant::now() + WAIT;
                while !vm.jit_compile_queue.has_finished() {
                    assert!(Instant::now() < deadline, "yielded bake did not finish");
                    std::thread::yield_now();
                }
                vm.install_finished_optimized_compiles();
                let report = vm.take_jit_debug_report().unwrap();
                assert!(!report.truncated());
                assert_eq!(report.dropped_events(), 0);
                let json = serde_json::to_value(report).unwrap();
                let events = json["events"].as_array().unwrap();
                assert_eq!(
                    events.len(),
                    1,
                    "abandoned prepared events remain unpublished"
                );
                assert_eq!(events[0]["type"], "compileFinished");
                assert_eq!(events[0]["preHookExit"]["kind"], "readRightYield");
                assert_eq!(events[0]["preHookExit"]["point"], yield_point_json);
                assert!(events[0]["compileStartedNs"].is_null());
                assert_eq!(events[0]["compileDurationNs"], 0);
                assert_eq!(events[0]["outcome"]["kind"], "cancelled");
            } else {
                let (pending, result) = finished(&mut vm.jit_compile_queue);
                assert_eq!(
                    result.pre_hook_exit,
                    Some(JitDebugPreHookExit::ReadRightYield { point: yield_point })
                );
                assert!(matches!(
                    result.status,
                    Ok(jit::JitCompileStatus::Cancelled)
                ));
                assert_eq!(result.duration, Duration::ZERO);
                assert_eq!(hook.entries.load(Ordering::Relaxed), 0);
                assert!(
                    weak.upgrade().is_some(),
                    "ACK owner still retains the rewritten cell"
                );
                drop(pending);
            }
            assert_eq!(hook.entries.load(Ordering::Relaxed), 0);
            assert!(
                weak.upgrade().is_none(),
                "consuming ACK releases the pending owner"
            );
            assert!(vm.jit_compile_queue.take_finished().is_empty());
            assert_eq!(
                vm.jit_compile_queue.pending_count(),
                0,
                "ACK does not resubmit the target"
            );
        }
        // A new request, made after the pause, must read the current heap and
        // enter its hook. GC stress and the background queue remain enabled.
        let (bake, pending) = job(&vm, &context, tier, 4, None, None);
        assert!(
            vm.jit_compile_queue
                .submit(hook.clone(), CompileInput::Bake(Box::new(bake)), 4, pending)
                .is_none()
        );
        let (pending, result) = finished(&mut vm.jit_compile_queue);
        assert_eq!(result.pre_hook_exit, None);
        assert!(matches!(
            result.status,
            Ok(jit::JitCompileStatus::Unavailable)
        ));
        assert_eq!(hook.entries.load(Ordering::Relaxed), 1);
        let products = pending.products.take();
        assert!(products.sources.as_ref().unwrap().is_current(
            context.space(),
            &pending.function,
            &[]
        ));
    }
}

#[test]
fn shared_table_contention_is_distinct_from_a_heap_pause() {
    for tier in [JitDebugTier::Template, JitDebugTier::Optimizing] {
        let (mut vm, context) = fixture();
        let hook = Arc::new(RecordingHook::default());
        let (bake, pending) = job(&vm, &context, tier, 1, None, None);
        let shared = vm.compile_shared.clone();
        let write = shared.function_realm_ids.get_mut(&mut vm.mutator);
        assert!(
            vm.jit_compile_queue
                .submit(hook.clone(), CompileInput::Bake(Box::new(bake)), 1, pending)
                .is_none()
        );
        let (_pending, result) = finished(&mut vm.jit_compile_queue);
        drop(write);
        assert_eq!(
            result.pre_hook_exit,
            Some(JitDebugPreHookExit::SharedStateContended)
        );
        assert_eq!(hook.entries.load(Ordering::Relaxed), 0);
        let (bake, pending) = job(&vm, &context, tier, 2, None, None);
        assert!(
            vm.jit_compile_queue
                .submit(hook.clone(), CompileInput::Bake(Box::new(bake)), 2, pending)
                .is_none()
        );
        let (_, result) = finished(&mut vm.jit_compile_queue);
        assert_eq!(result.pre_hook_exit, None);
        assert_eq!(hook.entries.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn root_refusal_distinguishes_cancellation_and_missing_source() {
    let (vm, context) = fixture();
    for tier in [JitDebugTier::Template, JitDebugTier::Optimizing] {
        let (bake, _pending) = job(&vm, &context, tier, 1, None, None);
        bake.cancellation.cancel();
        assert!(matches!(bake.run(), Err(JitDebugPreHookExit::Cancelled)));
        let (mut bake, _pending) = job(&vm, &context, tier, 2, None, None);
        bake.function_id = u32::MAX;
        assert!(matches!(
            bake.run(),
            Err(JitDebugPreHookExit::SourceUnavailable)
        ));
    }
}

#[test]
fn mandatory_literal_refusal_does_not_refuse_an_optional_graph_literal() {
    let mut vm = Interpreter::new().unwrap();
    let mut module = crate::test_support::minimal_bytecode_module("cold-required-literal.js");
    module.constants = vec![otter_bytecode::Constant::String {
        utf16: "cold".encode_utf16().collect(),
    }];
    module.functions[0].locals = 1;
    let mut code = otter_bytecode::FunctionCodeBuilder::new();
    code.push(
        Op::LoadString,
        &[
            otter_bytecode::Operand::Register(0),
            otter_bytecode::Operand::ConstIndex(0),
        ],
    );
    code.push(Op::ReturnValue, &[otter_bytecode::Operand::Register(0)]);
    module.functions[0].code = code.finish();
    let context = vm
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .unwrap();
    let (bake, _pending) = job(&vm, &context, JitDebugTier::Template, 1, None, None);
    assert!(matches!(
        bake.run(),
        Err(JitDebugPreHookExit::LiteralCellUnavailable)
    ));
    let (bake, _pending) = job(&vm, &context, JitDebugTier::Optimizing, 2, None, None);
    assert!(
        bake.run().is_ok(),
        "Graph may retain its generic literal operation"
    );
}

#[test]
fn lazy_body_records_its_refusal_before_a_later_contended_read() {
    let mut vm = Interpreter::new().unwrap();
    let mut module = crate::test_support::minimal_bytecode_module("unlinked-pre-hook.js");
    module.functions[0].lazy = Some(otter_bytecode::LazyFunction {
        range: (0, 14),
        form: otter_bytecode::LazyForm::Function,
        outer_strict: false,
        has_home_object: false,
        static_home: false,
        self_binding: false,
        dot_arguments_observed: false,
        in_field_initializer: false,
        module_goal: false,
        typescript: false,
        jsx: false,
    });
    module.function_source = Some("function f(){}".into());
    let context = vm
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .unwrap();
    let env = crate::interp::jit_bake_env::BakeEnv::capture(&vm);
    let mut baker = Baker::new(&env, None);
    assert!(baker.unlinked_body(&context, context.function_base()));
    assert_eq!(baker.abandoned, Some(JitDebugPreHookExit::UnlinkedBody));
    // An actual write lock, not an injected reason, makes the subsequent
    // compiler-side shared-state lookup contend.
    let shared = vm.compile_shared.clone();
    let write = shared.function_realm_ids.get_mut(&mut vm.mutator);
    env.function_realm_id(context.function_base());
    drop(write);
    assert!(env.contended());
    assert!(baker.yielded(JitDebugBakeYieldPoint::RootStart));
    assert_eq!(baker.abandoned, Some(JitDebugPreHookExit::UnlinkedBody));
}

#[test]
fn cancellation_phase_survives_terminal_ack_and_serialization() {
    for tier in [JitDebugTier::Template, JitDebugTier::Optimizing] {
        for capture in [false, true] {
            for before_hook in [true, false] {
                let (mut vm, context) = fixture();
                vm.set_jit_debug_request(JitDebugRequest::disabled().with_events(capture));
                let hook = Arc::new(RecordingHook {
                    entries: AtomicUsize::new(0),
                    cancel_on_entry: !before_hook,
                });
                let root = Arc::new(CalleeIdentityCell::holding(Value::number_i32(913)));
                let weak = Arc::downgrade(&root);
                let (ready, baked) = mpsc::channel();
                let (release, resume) = mpsc::channel();
                let (mut bake, pending) = job(&vm, &context, tier, 1, Some(root), None);
                if before_hook {
                    bake.before_hook = Some(BeforeHook { ready, resume });
                }
                let token = pending.cancellation.clone();
                assert!(
                    vm.jit_compile_queue
                        .submit(hook.clone(), CompileInput::Bake(Box::new(bake)), 1, pending)
                        .is_none()
                );
                if before_hook {
                    baked
                        .recv_timeout(WAIT)
                        .expect("bake publishes products before hook entry");
                    token.cancel();
                    assert!(
                        weak.upgrade().is_some(),
                        "pending products survive cancellation"
                    );
                    release.send(()).unwrap();
                }
                let deadline = Instant::now() + WAIT;
                while !vm.jit_compile_queue.has_finished() {
                    assert!(Instant::now() < deadline, "cancelled worker did not finish");
                    std::thread::yield_now();
                }
                assert!(
                    weak.upgrade().is_some(),
                    "finished result retains its pending owner"
                );
                // Exercise the production tier-specific finish methods and the
                // actual bounded diagnostics producer, not a hand-built DTO.
                vm.install_finished_optimized_compiles();
                assert_eq!(vm.jit_compile_queue.pending_count(), 0);
                assert!(
                    weak.upgrade().is_none(),
                    "terminal ACK releases product roots"
                );
                assert_eq!(
                    hook.entries.load(Ordering::Relaxed),
                    usize::from(!before_hook)
                );
                let report = vm.take_jit_debug_report();
                if !capture {
                    assert!(report.is_none(), "the real ACK path remains default-off");
                    continue;
                }
                let report = report.unwrap();
                assert!(!report.truncated());
                assert_eq!(report.dropped_events(), 0);
                let events = report
                    .events()
                    .iter()
                    .filter_map(|event| {
                        if let JitDebugEvent::CompileFinished {
                            function_id,
                            pre_hook_exit,
                            compile_started_ns,
                            outcome,
                            ..
                        } = event
                        {
                            Some((*function_id, *pre_hook_exit, *compile_started_ns, outcome))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].0, context.function_base());
                assert_eq!(
                    events[0].1,
                    before_hook.then_some(JitDebugPreHookExit::Cancelled)
                );
                assert_eq!(events[0].2.is_none(), before_hook);
                assert!(matches!(events[0].3, JitDebugCompileOutcome::Cancelled));
                let json = serde_json::to_value(report).unwrap();
                let event = json["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|event| event["type"] == "compileFinished")
                    .unwrap();
                assert_eq!(
                    event["preHookExit"],
                    if before_hook {
                        serde_json::json!({"kind": "cancelled"})
                    } else {
                        serde_json::Value::Null
                    }
                );
                assert_eq!(event["compileStartedNs"].is_null(), before_hook);
                assert_eq!(event["outcome"]["kind"], "cancelled");
            }
        }
    }
}
