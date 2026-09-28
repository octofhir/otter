//! Warm a callee through many short top-level scripts.
//!
//! # Contents
//! - [`warm`]: run one call statement `calls` times, split across scripts
//!   whose loops each run [`SHORT_LOOP`] iterations.
//! - [`WarmRuns`]: the per-script results, searched as one artifact/event set.
//!
//! # Invariants
//! - A script body's own loop never runs long enough to reach the optimizing
//!   tier, so it cannot inline the callee into an OSR-compiled script body:
//!   the callee reaches Template and Machine through its own entries, the way
//!   a function called from many cold call sites does.
//! - The loop variable `i` counts calls across scripts (`0..calls`), so a
//!   statement that varies its arguments with `i` sees the same sequence a
//!   single long loop would.

// Each integration test crate includes this module and uses a subset of it.
#![allow(dead_code)]

use otter_runtime::{ExecutionResult, JitArtifactBundle, JitDebugEvent, Runtime, SourceInput};

/// Iterations of one warm script's loop.
pub const SHORT_LOOP: usize = 12;

/// Results of one warm-up, in script order.
pub struct WarmRuns {
    runs: Vec<ExecutionResult>,
}

impl WarmRuns {
    /// Every compile artifact bundle captured by any warm script.
    pub fn bundles(&self) -> impl Iterator<Item = &JitArtifactBundle> {
        self.runs
            .iter()
            .filter_map(ExecutionResult::jit_artifacts)
            .flat_map(|batch| batch.bundles())
    }

    /// Every JIT debug event captured by any warm script.
    pub fn events(&self) -> impl Iterator<Item = &JitDebugEvent> {
        self.runs
            .iter()
            .filter_map(ExecutionResult::jit_debug_report)
            .flat_map(|report| report.events())
    }
}

/// Execute `statement` (which may read the call index `i`) `calls` times.
pub fn warm(runtime: &mut Runtime, statement: &str, calls: usize, module: &str) -> WarmRuns {
    let mut runs = Vec::new();
    let mut start = 0;
    while start < calls {
        let end = (start + SHORT_LOOP).min(calls);
        let source = format!("for (let i = {start}; i < {end}; i++) {{ {statement} }}");
        runs.push(
            runtime
                .run_script(SourceInput::from_javascript(source), module)
                .unwrap_or_else(|error| panic!("warm script {module}@{start}: {error:?}")),
        );
        start = end;
    }
    WarmRuns { runs }
}
