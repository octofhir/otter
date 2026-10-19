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
//! - A compilation whose function was invalidated after its snapshot is
//!   dropped when it finishes.
//! - With no compiler threads the requesting thread compiles at once and the
//!   caller installs the result before the request returns.
//! - Compiler threads start on demand: one more whenever the compilations in
//!   flight outnumber the threads, up to the configured count.
//! - Compiler threads belong to the isolate: dropping the queue stops and
//!   joins them, and requests not yet started are dropped. A compiler panic
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

use crate::jit::{JitCompileError, JitCompileRequest, JitCompileStatus, JitCompilerHook};
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
    /// The function's feedback epoch when the snapshot was taken.
    pub(crate) feedback_epoch: Option<u32>,
    /// Debug-clock time of the request, when events are captured.
    pub(crate) queued_ns: u64,
    /// Name recorded in the perf map, when one is written.
    pub(crate) perf_name: Option<String>,
    /// Set when the function was invalidated after the snapshot.
    pub(crate) stale: bool,
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
        {
            let mut queue = self.shared.lock();
            queue.stopping = true;
            queue.requests.clear();
        }
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
    let status = job.hook.compile_optimized_function(job.request);
    FinishedCompile {
        code_object_id,
        status,
        queue_delay: started.saturating_duration_since(job.queued),
        duration: started.elapsed(),
    }
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

    /// Mark the compilations of every function in `functions` stale: their
    /// snapshots predate an invalidation.
    pub(crate) fn invalidate(&mut self, functions: &[u32]) {
        for function_id in functions {
            if let Some(id) = self.pending_functions.get(function_id)
                && let Some(pending) = self.pending.get_mut(id)
            {
                pending.stale = true;
            }
        }
    }

    /// Visit the roots of every compilation in flight.
    pub(crate) fn trace_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for pending in self.pending.values() {
            for root in pending.roots.iter() {
                root.trace(visitor);
            }
        }
    }
}
