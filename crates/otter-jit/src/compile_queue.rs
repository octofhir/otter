//! Off-thread compilation queue.
//!
//! The main thread enqueues compilation requests (with frozen feedback
//! snapshots), and a background thread performs MIR construction, optimization
//! passes, and codegen. When compilation finishes, the main thread installs
//! the compiled code.
//!
//! ## Architecture (SpiderMonkey Warp model)
//!
//! ```text
//! Main Thread              Background Thread
//! ───────────              ─────────────────
//! 1. Snapshot feedback
//!    (fast, < 1ms)
//! 2. Enqueue request  →    3. Build MIR from snapshot
//!                          4. Run optimization passes
//!                          5. Lower to CLIF → native code
//! 6. Install code     ←    (compilation complete)
//!    Patch call sites
//! ```
//!
//! Spec: Phase 7.1 of JIT_INCREMENTAL_PLAN.md

use std::collections::VecDeque;

use otter_vm::feedback::FeedbackVector;

use crate::Tier;

// ============================================================
// Compile Request
// ============================================================

/// A request to compile a function, with frozen feedback.
#[derive(Debug, Clone)]
pub struct CompileRequest {
    /// Function name (for diagnostics).
    pub function_name: String,
    /// Module-level function index.
    pub function_index: u32,
    /// Target tier for compilation.
    pub tier: Tier,
    /// Frozen snapshot of the feedback vector at request time.
    /// The main thread can continue modifying its live FeedbackVector
    /// while the background thread uses this frozen copy.
    pub feedback_snapshot: FeedbackVector,
    /// Priority (higher = compile sooner). Based on backedge count.
    pub priority: u32,
}

// ============================================================
// Compile Result
// ============================================================

/// Result of a background compilation.
#[derive(Debug)]
pub enum CompileResult {
    /// Compilation succeeded — ready to install.
    Success {
        function_index: u32,
        tier: Tier,
        code_size: usize,
        compile_time_ns: u64,
    },
    /// Compilation failed (non-fatal — function stays in interpreter).
    Failed {
        function_index: u32,
        error: String,
    },
}

// ============================================================
// Compilation Queue
// ============================================================

/// Queue state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    /// Queue is idle, no pending work.
    Idle,
    /// Compilation in progress.
    Compiling,
    /// Queue is shut down (runtime teardown).
    Shutdown,
}

/// Thread-local compilation queue for the single-threaded VM.
///
/// In the current single-threaded model, "off-thread" compilation is actually
/// synchronous — but the queue abstraction allows easy migration to true
/// background compilation when Otter gets a compilation thread.
///
/// The key invariant: feedback is **snapshot** at enqueue time, so compilation
/// uses a frozen copy that doesn't race with the interpreter.
#[derive(Debug)]
pub struct CompileQueue {
    /// Pending compilation requests, ordered by priority.
    pending: VecDeque<CompileRequest>,
    /// Completed compilations awaiting installation.
    completed: Vec<CompileResult>,
    /// Current state.
    state: QueueState,
    /// Maximum pending requests before dropping low-priority ones.
    max_pending: usize,
}

impl CompileQueue {
    /// Create a new compilation queue.
    #[must_use]
    pub fn new(max_pending: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            completed: Vec::new(),
            state: QueueState::Idle,
            max_pending,
        }
    }

    /// Enqueue a compilation request.
    ///
    /// If the queue is full, the lowest-priority request is dropped.
    pub fn enqueue(&mut self, request: CompileRequest) {
        if self.state == QueueState::Shutdown {
            return;
        }

        // Don't enqueue duplicates for the same function + tier.
        if self.pending.iter().any(|r| {
            r.function_index == request.function_index && r.tier == request.tier
        }) {
            return;
        }

        self.pending.push_back(request);

        // If over capacity, drop the lowest-priority request.
        if self.pending.len() > self.max_pending {
            // Find minimum priority.
            if let Some(min_idx) = self
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.priority)
                .map(|(i, _)| i)
            {
                self.pending.remove(min_idx);
            }
        }
    }

    /// Dequeue the next request to compile (highest priority first).
    pub fn dequeue(&mut self) -> Option<CompileRequest> {
        if self.pending.is_empty() {
            return None;
        }
        // Find highest priority.
        let max_idx = self
            .pending
            .iter()
            .enumerate()
            .max_by_key(|(_, r)| r.priority)
            .map(|(i, _)| i)?;
        self.pending.remove(max_idx)
    }

    /// Submit a compilation result (called after compilation finishes).
    pub fn submit_result(&mut self, result: CompileResult) {
        self.completed.push(result);
    }

    /// Drain all completed compilations for installation on the main thread.
    pub fn drain_completed(&mut self) -> Vec<CompileResult> {
        std::mem::take(&mut self.completed)
    }

    /// Number of pending requests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Number of completed (not yet installed) results.
    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.completed.len()
    }

    /// Whether the queue has any work to do.
    #[must_use]
    pub fn has_work(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Shut down the queue (called on runtime teardown).
    pub fn shutdown(&mut self) {
        self.state = QueueState::Shutdown;
        self.pending.clear();
    }
}

impl Default for CompileQueue {
    fn default() -> Self {
        Self::new(32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::feedback::FeedbackVector;

    fn make_request(index: u32, priority: u32) -> CompileRequest {
        CompileRequest {
            function_name: format!("fn_{index}"),
            function_index: index,
            tier: Tier::Baseline,
            feedback_snapshot: FeedbackVector::empty(),
            priority,
        }
    }

    #[test]
    fn test_enqueue_dequeue_priority() {
        let mut q = CompileQueue::new(10);
        q.enqueue(make_request(1, 10));
        q.enqueue(make_request(2, 50)); // highest priority
        q.enqueue(make_request(3, 30));

        let next = q.dequeue().unwrap();
        assert_eq!(next.function_index, 2); // highest priority first
        assert_eq!(q.pending_count(), 2);
    }

    #[test]
    fn test_no_duplicates() {
        let mut q = CompileQueue::new(10);
        q.enqueue(make_request(1, 10));
        q.enqueue(make_request(1, 20)); // same function + tier → deduplicated
        assert_eq!(q.pending_count(), 1);
    }

    #[test]
    fn test_capacity_overflow_drops_lowest() {
        let mut q = CompileQueue::new(3);
        q.enqueue(make_request(1, 10));
        q.enqueue(make_request(2, 50));
        q.enqueue(make_request(3, 30));
        // Queue full (3). Adding one more drops lowest priority.
        q.enqueue(make_request(4, 40));

        assert_eq!(q.pending_count(), 3);
        // fn_1 (priority 10) should have been dropped.
        assert!(!q.pending.iter().any(|r| r.function_index == 1));
    }

    #[test]
    fn test_submit_and_drain() {
        let mut q = CompileQueue::new(10);
        q.submit_result(CompileResult::Success {
            function_index: 1,
            tier: Tier::Baseline,
            code_size: 256,
            compile_time_ns: 5000,
        });
        q.submit_result(CompileResult::Failed {
            function_index: 2,
            error: "unsupported".into(),
        });

        let results = q.drain_completed();
        assert_eq!(results.len(), 2);
        assert_eq!(q.completed_count(), 0); // drained
    }

    #[test]
    fn test_shutdown_rejects() {
        let mut q = CompileQueue::new(10);
        q.shutdown();
        q.enqueue(make_request(1, 10));
        assert_eq!(q.pending_count(), 0); // rejected
    }
}
