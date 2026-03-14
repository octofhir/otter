//! # Otter VM Exec
//!
//! Execution-tier coordination for baseline JIT:
//! - hot-function queueing
//! - compilation lifecycle
//! - JIT entry lookup and bailout/deopt accounting

#![warn(clippy::all)]
#![warn(missing_docs)]

mod entry_stub;
mod jit_queue;
mod jit_runtime;

pub use jit_queue::{clear_for_tests, enqueue_hot_function, pending_count};
pub use jit_runtime::{
    DeoptFrameSnapshot, DeoptResumeMode, JitBailoutSiteStat, JitExecResult, JitHelperCallStat,
    JitRuntimeStats, compile_one_pending_request, compile_one_pending_request_sync,
    deopt_metadata_snapshot, hydrate_jit_entry_ptr,
    invalidate_jit_code, is_jit_background_enabled, is_jit_enabled,
    jit_deopt_threshold, jit_hot_threshold, record_back_edge_compilation, record_osr_attempt,
    record_osr_success, stats_snapshot, try_execute_jit_raw,
};

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
