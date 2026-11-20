//! The isolate's optimizing compiler threads.
//!
//! An optimizing compilation has three parts. The isolate thread prepares the
//! facts the function's compilation needs and queues a bake. A compiler
//! thread bakes the [`crate::jit::JitCompileSnapshot`] under the heap read
//! right (every heap fact frozen into plain data and addresses of non-moving
//! cells), leaves the bake's roots, install tasks and source leases in the
//! compilation's [`BakeProducts`], releases the right and compiles without
//! touching the heap. The isolate thread installs the result at its next
//! tier decision or loop poll.
//!
//! # Contents
//! - [`OptimizingCompileQueue`] — the isolate's compiler threads, the requests
//!   they take, the results they hand back, and the isolate-thread record of
//!   every compilation in flight.
//! - [`pool`] — worker lifetime and synchronized dispatch.
//! - [`PendingCompile`] — what the isolate keeps for one request until its
//!   result is installed or dropped.
//! - [`FinishedCompile`] — one request's compiler outcome and its timing.
//!
//! # Invariants
//! - A compiler thread reads heap memory only while baking, under the heap
//!   read right, which every collection pause excludes; the request it then
//!   compiles holds no GC handle: the snapshot names hidden classes, cells
//!   and literals by addresses of non-moving allocations. The isolate traces
//!   the products of every pending compilation (the bake's captured handles
//!   from the start, its roots once published), so those allocations live
//!   until the result is installed (the installed code then takes the roots
//!   over) or dropped.
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
//!   flight outnumber the threads, up to the configured count. If the first
//!   worker cannot start, the requesting thread compiles that job. If pool
//!   growth fails, existing workers drain the queue.
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

mod pool;
use pool::CompilerPool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::JitCompileCancellation;
#[cfg(test)]
use crate::jit::JitCompileRequest;
use crate::jit::{JitCompileError, JitCompileStatus, JitCompilerHook};
use crate::jit_compile_lifetime::CompilationSources;
use crate::jit_roots::CompilationRoot;

/// What the isolate keeps for one compilation in flight.
pub(crate) struct PendingCompile {
    /// Function being compiled.
    pub(crate) function_id: u32,
    /// Loop header that requested the compilation, if any.
    pub(crate) osr_pc: Option<u32>,
    /// The tier being compiled, whose installation runs at collection.
    pub(crate) tier: crate::jit_debug::JitDebugTier,
    /// The exact body the snapshot is taken from.
    pub(crate) function: Arc<crate::executable::CodeBlock>,
    /// The roots, install tasks, events and source leases the bake leaves.
    pub(crate) products: Arc<BakeProducts>,
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
    /// First preparation refusal; `None` exactly when the hook was entered.
    pub(crate) pre_hook_exit: Option<crate::JitDebugPreHookExit>,
}

/// What a compiler thread starts a compilation from.
pub(crate) enum CompileInput {
    /// A snapshot already baked.
    #[cfg(test)]
    Request(Box<JitCompileRequest>),
    /// A snapshot the compiler thread bakes first.
    Bake(Box<crate::interp::jit_bake::BakeJob>),
}

struct CompileJob {
    hook: Arc<dyn JitCompilerHook>,
    tier: crate::jit_debug::JitDebugTier,
    input: CompileInput,
    cancellation: JitCompileCancellation,
    code_object_id: u64,
    queued: Instant,
}

/// What one compilation's bake leaves for the isolate: written by the
/// compiler thread while it holds the heap read right, then read by the
/// isolate thread, whose collector traces the roots throughout.
#[derive(Default)]
pub(crate) struct BakeProducts(Mutex<BakeOutput>);

/// [`BakeProducts`]' contents.
#[derive(Default)]
pub(crate) struct BakeOutput {
    /// Every allocation the snapshot names, and the bake's captured handles.
    pub(crate) roots: Vec<CompilationRoot>,
    pub(crate) install: crate::interp::jit_bake::InstallTasks,
    pub(crate) events: Vec<crate::jit_debug::JitDebugEvent>,
    /// Exact source chunks of all prepared bodies, until terminal ACK;
    /// `None` until a bake succeeds.
    pub(crate) sources: Option<CompilationSources>,
}

// SAFETY: the roots name GC cells; the compiler thread writes them only under
// the heap read right, and the collector, on the isolate thread, reads and
// rewrites them only during pauses, which exclude that right.
unsafe impl Send for BakeProducts {}
// SAFETY: as above; every access holds the mutex.
unsafe impl Sync for BakeProducts {}

impl BakeProducts {
    fn lock(&self) -> MutexGuard<'_, BakeOutput> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Products holding `roots` from the start.
    pub(crate) fn holding(roots: impl IntoIterator<Item = CompilationRoot>) -> Self {
        Self(Mutex::new(BakeOutput {
            roots: roots.into_iter().collect(),
            ..BakeOutput::default()
        }))
    }

    /// Leave a finished bake's output.
    pub(crate) fn publish(
        &self,
        roots: Box<[CompilationRoot]>,
        install: crate::interp::jit_bake::InstallTasks,
        events: Vec<crate::jit_debug::JitDebugEvent>,
        sources: CompilationSources,
    ) {
        let mut output = self.lock();
        output.roots.extend(roots.into_vec());
        output.install = install;
        output.events = events;
        output.sources = Some(sources);
    }

    /// Take everything left, for installation.
    pub(crate) fn take(&self) -> BakeOutput {
        std::mem::take(&mut *self.lock())
    }

    fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for root in &self.lock().roots {
            root.trace(visitor);
        }
    }

    fn process_weak(
        &self,
        resolve: &mut dyn FnMut(otter_gc::raw::RawGc) -> Option<otter_gc::raw::RawGc>,
    ) {
        for root in &self.lock().roots {
            root.process_weak(resolve);
        }
    }
}

fn compile(job: CompileJob) -> FinishedCompile {
    let started = Instant::now();
    let code_object_id = job.code_object_id;
    let cancellation = job.cancellation;
    let request = if cancellation.is_cancelled() {
        Err(crate::JitDebugPreHookExit::Cancelled)
    } else {
        match job.input {
            #[cfg(test)]
            CompileInput::Request(request) => Ok(*request),
            CompileInput::Bake(bake) => bake.run(),
        }
    };
    // Keep an earlier bake refusal if cancellation raced with that refusal.
    // A successful bake must still recheck the token before entering the hook.
    let request = request.and_then(|request| {
        if cancellation.is_cancelled() {
            Err(crate::JitDebugPreHookExit::Cancelled)
        } else {
            Ok(request)
        }
    });
    let pre_hook_exit = request.as_ref().err().copied();
    let status = if let Ok(request) = request {
        let status = match job.tier {
            crate::jit_debug::JitDebugTier::Template => job.hook.compile_function(request),
            _ => job.hook.compile_optimized_function(request),
        };
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
        duration: if pre_hook_exit.is_none() {
            started.elapsed()
        } else {
            Duration::ZERO
        },
        pre_hook_exit,
    }
}

/// A queued job's terminal ACK follows destruction of its snapshot.
fn cancel_queued_job(job: CompileJob) -> FinishedCompile {
    let finished = FinishedCompile {
        code_object_id: job.code_object_id,
        status: Ok(JitCompileStatus::Cancelled),
        queue_delay: job.queued.elapsed(),
        duration: Duration::ZERO,
        pre_hook_exit: Some(crate::JitDebugPreHookExit::Cancelled),
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
    /// Jobs whose roots and source leases have not been acknowledged by the
    /// isolate, including worker results awaiting its next collection poll.
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

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
        self.pool.as_ref().is_some_and(CompilerPool::has_finished)
    }

    /// Hand `request` to the compiler. With no compiler threads it compiles
    /// here and returns the finished compilation with its pending record;
    /// otherwise it returns `None` and the result is collected later by
    /// [`Self::take_finished`].
    pub(crate) fn submit(
        &mut self,
        hook: Arc<dyn JitCompilerHook>,
        input: CompileInput,
        code_object_id: u64,
        pending: PendingCompile,
    ) -> Option<(PendingCompile, FinishedCompile)> {
        let job = CompileJob {
            hook,
            tier: pending.tier,
            input,
            cancellation: pending.cancellation.clone(),
            code_object_id,
            queued: Instant::now(),
        };
        if self.threads == 0 {
            return Some((pending, compile(job)));
        }
        let in_flight = self.pending.len() + 1;
        let pool = self.pool.get_or_insert_with(CompilerPool::new);
        if !pool.ensure_worker(self.threads.min(in_flight)) {
            return Some((pending, compile(job)));
        }
        self.pending_functions
            .insert(pending.function_id, code_object_id);
        self.pending.insert(code_object_id, pending);
        pool.enqueue(job);
        None
    }

    /// Collect every result the compiler threads handed back, each with its
    /// pending record.
    pub(crate) fn take_finished(&mut self) -> Vec<(PendingCompile, FinishedCompile)> {
        let Some(pool) = &self.pool else {
            return Vec::new();
        };
        let results = pool.take_results();
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
        if let Some(pool) = &self.pool {
            pool.acknowledge_queued_cancellations();
        }
    }

    /// Cancel every request for teardown, source-space replacement or pressure.
    pub(crate) fn cancel_all(&mut self) {
        for pending in self.pending.values() {
            pending.cancellation.cancel();
        }
        if let Some(pool) = &self.pool {
            pool.acknowledge_queued_cancellations();
        }
    }

    /// Mark only cancelled owners that outlived an actual pressure census.
    pub(crate) fn mark_cancelled_for_reclamation_retry(&mut self) {
        for pending in self.pending.values_mut() {
            if pending.cancellation.is_cancelled() {
                pending.retry_code_reclamation = true;
            }
        }
    }

    /// Visit the roots of every compilation in flight.
    pub(crate) fn trace_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for pending in self.pending.values() {
            pending.products.trace(visitor);
        }
    }

    /// Weak lookup caches belong to the same pending session as its strong
    /// constants, but do not retain the JavaScript values they observed.
    pub(crate) fn process_weak(
        &self,
        resolve: &mut dyn FnMut(otter_gc::raw::RawGc) -> Option<otter_gc::raw::RawGc>,
    ) {
        for pending in self.pending.values() {
            pending.products.process_weak(resolve);
        }
    }
}

#[cfg(test)]
mod tests;
