//! The isolate's optimizing compiler threads.
//!
//! An optimizing compilation has three parts. The isolate thread prepares a
//! [`crate::jit::JitCompileSnapshot`]: every heap fact the compiler may use,
//! frozen into plain data and addresses of non-moving cells. A compiler
//! thread turns that request into code without touching the heap. The
//! isolate thread installs the result at its next tier decision or loop poll.
//!
//! # Contents
//! - [`OptimizingCompileQueue`] — the isolate's compiler threads, the requests
//!   they take, the results they hand back, and the isolate-thread record of
//!   every compilation in flight.
//! - [`PendingCompile`] — what the isolate keeps for one request until its
//!   result is installed or dropped.
//! - [`FinishedCompile`] — one request's compiler outcome and its timing.
//!
//! # Invariants
//! - A request holds no GC handle and the compiler reads no heap memory: the
//!   snapshot names hidden classes, cells and literals by addresses of
//!   non-moving allocations. The isolate traces the roots of every pending
//!   compilation, so those allocations live until the result is installed
//!   (the installed code then takes the roots over) or dropped.
//! - A function has at most one compilation in flight.
//! - Results are installed only on the isolate thread. Installed code makes no
//!   assumption its snapshot could have outlived: every speculation is guarded
//!   in the code itself, and dependency epochs are checked at admission.
//! - Cancellation is one monotonic token shared by the pending owner and
//!   request. Queued cancellations skip the hook; active compilers cooperate
//!   at phase boundaries. Every path acknowledges before releasing roots and
//!   source leases, and installation checks the token again.
//! - With no compiler threads the requesting thread compiles at once and the
//!   caller installs the result before the request returns.
//! - Compiler threads start on demand: one more whenever the compilations in
//!   flight outnumber the threads, up to the configured count.
//! - Compiler threads belong to the isolate: dropping the queue stops and
//!   cancels and joins them, and requests not yet started are dropped outside
//!   the queue lock. A compiler panic
//!   resumes on the isolate thread when its result is collected.
//! - Finished results wait for the isolate's next loop poll or tier decision;
//!   compiled loops poll after a bounded batch of back-edges.
//!
//! # See also
//! - [`crate::jit::JitCompilerHook`] — the compiler a request runs.
//! - [`crate::jit_roots`] — the roots a pending compilation retains.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::JitCompileCancellation;
use crate::jit::{JitCompileError, JitCompileRequest, JitCompileStatus, JitCompilerHook};
use crate::jit_compile_lifetime::CompilationSources;
use crate::jit_roots::CompilationRoot;

/// Stack reserved for each compiler thread: the same depth the isolate thread
/// gives a compilation that runs on it.
const COMPILER_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

/// What the isolate keeps for one compilation in flight.
pub(crate) struct PendingCompile {
    /// Function being compiled.
    pub(crate) function_id: u32,
    /// Loop header that requested the compilation, if any.
    pub(crate) osr_pc: Option<u32>,
    /// The exact body the snapshot was taken from.
    pub(crate) function: Arc<crate::executable::CodeBlock>,
    /// Every allocation the snapshot names.
    pub(crate) roots: Box<[CompilationRoot]>,
    /// Exact source chunks of all prepared bodies, until terminal ACK.
    pub(crate) sources: CompilationSources,
    /// The same stop token carried by the queued or active request.
    pub(crate) cancellation: JitCompileCancellation,
    /// A pressure census deferred reclamation while this job held source
    /// leases. Its terminal ACK re-arms the existing between-turn watermark.
    pub(crate) retry_code_reclamation: bool,
    /// The function's feedback epoch when the snapshot was taken.
    pub(crate) feedback_epoch: Option<u32>,
    /// Debug-clock time of the request, when events are captured.
    pub(crate) queued_ns: u64,
    /// Name recorded in the perf map, when one is written.
    pub(crate) perf_name: Option<String>,
}

/// One compilation's outcome, handed from the compiler back to the isolate.
pub(crate) struct FinishedCompile {
    /// Identity the request reserved for its code object.
    pub(crate) code_object_id: u64,
    /// The compiler's answer.
    pub(crate) status: Result<JitCompileStatus, JitCompileError>,
    /// Time between the request and the compiler starting on it.
    pub(crate) queue_delay: Duration,
    /// Time the compiler spent on it.
    pub(crate) duration: Duration,
    /// False when cancellation acknowledged without entering the hook.
    pub(crate) entered_hook: bool,
}

struct CompileJob {
    hook: Arc<dyn JitCompilerHook>,
    request: JitCompileRequest,
    queued: Instant,
}

/// A compiler panic, carried to the isolate thread.
type CompilerPanic = Box<dyn std::any::Any + Send>;

#[derive(Default)]
struct PoolQueue {
    requests: VecDeque<CompileJob>,
    results: Vec<Result<FinishedCompile, CompilerPanic>>,
    stopping: bool,
}

struct PoolShared {
    queue: Mutex<PoolQueue>,
    wake: Condvar,
    /// Set while `queue.results` holds anything; read without the lock.
    finished: AtomicBool,
}

impl PoolShared {
    fn lock(&self) -> MutexGuard<'_, PoolQueue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct CompilerPool {
    shared: Arc<PoolShared>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl CompilerPool {
    fn new() -> Self {
        Self {
            shared: Arc::new(PoolShared {
                queue: Mutex::default(),
                wake: Condvar::new(),
                finished: AtomicBool::new(false),
            }),
            threads: Vec::new(),
        }
    }

    /// Start one more compiler thread.
    fn spawn_thread(&mut self) {
        let shared = Arc::clone(&self.shared);
        if let Ok(thread) = std::thread::Builder::new()
            .name(format!("otter-jit-compiler-{}", self.threads.len()))
            .stack_size(COMPILER_THREAD_STACK_BYTES)
            .spawn(move || compile_requests(&shared))
        {
            self.threads.push(thread);
        }
    }
}

impl Drop for CompilerPool {
    fn drop(&mut self) {
        let requests = {
            let mut queue = self.shared.lock();
            queue.stopping = true;
            std::mem::take(&mut queue.requests)
        };
        // Snapshot and hook destructors must never run under the queue lock.
        drop(requests);
        self.shared.wake.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// One compiler thread: take requests until the pool stops.
fn compile_requests(shared: &PoolShared) {
    loop {
        let job = {
            let mut queue = shared.lock();
            loop {
                if queue.stopping {
                    return;
                }
                if let Some(job) = queue.requests.pop_front() {
                    break job;
                }
                queue = shared
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| compile(job)));
        shared.lock().results.push(result);
        shared.finished.store(true, Ordering::Release);
    }
}

fn compile(job: CompileJob) -> FinishedCompile {
    let started = Instant::now();
    let code_object_id = job.request.code_object_id;
    let cancellation = job.request.cancellation.clone();
    let entered_hook = !cancellation.is_cancelled();
    let status = if entered_hook {
        let status = job.hook.compile_optimized_function(job.request);
        // A hook may ignore cancellation or finish its last phase just before
        // cancellation. A completed artifact still must not be published.
        if cancellation.is_cancelled() {
            Ok(JitCompileStatus::Cancelled)
        } else {
            status
        }
    } else {
        Ok(JitCompileStatus::Cancelled)
    };
    FinishedCompile {
        code_object_id,
        status,
        queue_delay: started.saturating_duration_since(job.queued),
        duration: if entered_hook {
            started.elapsed()
        } else {
            Duration::ZERO
        },
        entered_hook,
    }
}

/// A queued job's terminal ACK follows destruction of its snapshot.
fn cancel_queued_job(job: CompileJob) -> FinishedCompile {
    let finished = FinishedCompile {
        code_object_id: job.request.code_object_id,
        status: Ok(JitCompileStatus::Cancelled),
        queue_delay: job.queued.elapsed(),
        duration: Duration::ZERO,
        entered_hook: false,
    };
    drop(job);
    finished
}

/// The isolate's optimizing compilations: its compiler threads and every
/// request still in flight.
#[derive(Default)]
pub(crate) struct OptimizingCompileQueue {
    /// Compiler threads the pool starts with; zero compiles on the
    /// requesting thread.
    threads: usize,
    /// Started at the first request that goes to a compiler thread.
    pool: Option<CompilerPool>,
    /// Compilations in flight by reserved code-object identity.
    pending: rustc_hash::FxHashMap<u64, PendingCompile>,
    /// The one compilation in flight for each function.
    pending_functions: rustc_hash::FxHashMap<u32, u64>,
}

impl Drop for OptimizingCompileQueue {
    fn drop(&mut self) {
        self.cancel_all();
        // Join active readers while pending roots and source leases still
        // exist. Only after the join may ordinary field destruction free them.
        drop(self.pool.take());
    }
}

impl OptimizingCompileQueue {
    /// Compile on up to `threads` compiler threads; zero compiles on the
    /// requesting thread. A pool already started keeps its threads.
    pub(crate) fn set_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    /// Whether `function_id` has a compilation in flight.
    pub(crate) fn is_pending(&self, function_id: u32) -> bool {
        self.pending_functions.contains_key(&function_id)
    }

    /// Whether a compiler thread has handed back a result not yet collected.
    pub(crate) fn has_finished(&self) -> bool {
        self.pool
            .as_ref()
            .is_some_and(|pool| pool.shared.finished.load(Ordering::Acquire))
    }

    /// Hand `request` to the compiler. With no compiler threads it compiles
    /// here and returns the finished compilation with its pending record;
    /// otherwise it returns `None` and the result is collected later by
    /// [`Self::take_finished`].
    pub(crate) fn submit(
        &mut self,
        hook: Arc<dyn JitCompilerHook>,
        request: JitCompileRequest,
        pending: PendingCompile,
    ) -> Option<(PendingCompile, FinishedCompile)> {
        let code_object_id = request.code_object_id;
        let job = CompileJob {
            hook,
            request,
            queued: Instant::now(),
        };
        if self.threads == 0 {
            return Some((pending, compile(job)));
        }
        self.pending_functions
            .insert(pending.function_id, code_object_id);
        self.pending.insert(code_object_id, pending);
        let in_flight = self.pending.len();
        let pool = self.pool.get_or_insert_with(CompilerPool::new);
        if pool.threads.len() < self.threads.min(in_flight) {
            pool.spawn_thread();
        }
        pool.shared.lock().requests.push_back(job);
        pool.shared.wake.notify_one();
        None
    }

    /// Collect every result the compiler threads handed back, each with its
    /// pending record.
    pub(crate) fn take_finished(&mut self) -> Vec<(PendingCompile, FinishedCompile)> {
        let Some(pool) = &self.pool else {
            return Vec::new();
        };
        let results = {
            let mut queue = pool.shared.lock();
            pool.shared.finished.store(false, Ordering::Release);
            std::mem::take(&mut queue.results)
        };
        let mut finished = Vec::with_capacity(results.len());
        for result in results {
            let compiled = result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            let Some(pending) = self.pending.remove(&compiled.code_object_id) else {
                continue;
            };
            self.pending_functions.remove(&pending.function_id);
            finished.push((pending, compiled));
        }
        finished
    }

    /// Cancel compilations whose snapshots predate a function invalidation.
    pub(crate) fn invalidate(&mut self, functions: &[u32]) {
        for function_id in functions {
            if let Some(id) = self.pending_functions.get(function_id)
                && let Some(pending) = self.pending.get(id)
            {
                pending.cancellation.cancel();
            }
        }
        self.acknowledge_queued_cancellations();
    }

    /// Cancel every request for teardown, source-space replacement or pressure.
    pub(crate) fn cancel_all(&mut self) {
        for pending in self.pending.values() {
            pending.cancellation.cancel();
        }
        self.acknowledge_queued_cancellations();
    }

    /// Mark only cancelled owners that outlived an actual pressure census.
    pub(crate) fn mark_cancelled_for_reclamation_retry(&mut self) {
        for pending in self.pending.values_mut() {
            if pending.cancellation.is_cancelled() {
                pending.retry_code_reclamation = true;
            }
        }
    }

    fn acknowledge_queued_cancellations(&self) {
        let Some(pool) = &self.pool else {
            return;
        };
        let cancelled = {
            let mut queue = pool.shared.lock();
            let requests = std::mem::take(&mut queue.requests);
            let mut cancelled = Vec::new();
            for job in requests {
                if job.request.cancellation.is_cancelled() {
                    cancelled.push(job);
                } else {
                    queue.requests.push_back(job);
                }
            }
            cancelled
        };
        if cancelled.is_empty() {
            return;
        }
        let finished = cancelled
            .into_iter()
            .map(cancel_queued_job)
            .map(Ok)
            .collect::<Vec<_>>();
        pool.shared.lock().results.extend(finished);
        pool.shared.finished.store(true, Ordering::Release);
    }

    /// Visit the roots of every compilation in flight.
    pub(crate) fn trace_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for pending in self.pending.values() {
            for root in pending.roots.iter() {
                root.trace(visitor);
            }
        }
    }

    /// Weak lookup caches belong to the same pending session as its strong
    /// constants, but do not retain the JavaScript values they observed.
    pub(crate) fn process_weak(
        &self,
        resolve: &mut dyn FnMut(otter_gc::raw::RawGc) -> Option<otter_gc::raw::RawGc>,
    ) {
        for pending in self.pending.values() {
            for root in pending.roots.iter() {
                root.process_weak(resolve);
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
            function: snapshot.code_block.clone(),
            roots: vec![CompilationRoot::CalleeIdentity(root)].into_boxed_slice(),
            sources,
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
        assert!(queue.submit(hook.clone(), active, active_pending).is_none());
        assert_eq!(entries.recv().unwrap(), contexts[0].function_base());
        assert!(queue.submit(hook.clone(), queued, queued_pending).is_none());
        // Both chunks are retained only by the pending owners, not requests'
        // CodeBlock Arcs. The worker cannot complete before cancellation.
        drop(contexts);
        assert!(space.eviction_candidates().is_empty());
        queue.invalidate(&[queued_id]);
        assert!(
            queue.is_pending(queued_id),
            "pending survives until terminal ACK consumption"
        );
        assert!(queued_root.upgrade().is_some());
        queue.mark_cancelled_for_reclamation_retry();
        assert!(queue.pending[&2].retry_code_reclamation);
        assert!(!queue.pending[&1].retry_code_reclamation);
        let finished = queue.take_finished();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].0.function_id, queued_id);
        assert!(!finished[0].1.entered_hook);
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
}
