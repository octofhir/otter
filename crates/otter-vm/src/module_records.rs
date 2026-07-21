//! Cyclic Module Record evaluation state (§16.2.1.5).
//!
//! One [`ModuleRecordState`] per linked module URL, owned by the
//! [`Interpreter`] with the same lifecycle as `module_environments`.
//! The record map replaces the former ad-hoc quartet
//! (`module_evaluating`, `evaluated_modules`, `module_errors`,
//! `module_async_init_promises`) with the spec's per-record fields, so
//! every evaluation consumer — static graph, dynamic `import()`,
//! deferred namespaces — reads and settles through the same state.
//!
//! # Contents
//! - [`ModuleStatus`] — the spec's `[[Status]]` evaluation slice.
//! - [`ModuleRecordState`] — `[[Status]]` / `[[EvaluationError]]` /
//!   `[[TopLevelCapability]]`-shaped promise gate per module.
//! - Record accessors on [`Interpreter`].
//!
//! # Invariants
//! - `evaluation_promise` and `evaluation_error` are GC roots, traced
//!   from `RuntimeState::trace_roots`.
//! - A record with `evaluation_error` set is always `Evaluated`
//!   (§16.2.1.5 step 8: an abrupt completion transitions every module
//!   on the stack to `evaluated`).
//! - Records and module environments persist for the owning realm's lifetime,
//!   making evaluation idempotent across separate top-level entry graphs.
//!
//! # See also
//! - [`crate::module_ops`]
//! - <https://tc39.es/ecma262/#sec-cyclic-module-records>

use crate::{Interpreter, Value, promise::JsPromiseHandle};
use std::sync::Arc;

/// §16.2.1.4 `[[Status]]`, restricted to the evaluation phase the
/// interpreter drives (linking is done by the runtime's module graph
/// before execution starts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ModuleStatus {
    /// Linked, not yet started evaluating.
    #[default]
    New,
    /// `<module-init>` is on the synchronous evaluation stack.
    Evaluating,
    /// Body parked at top-level await, or waiting on async
    /// dependencies (`[[AsyncEvaluation]]` is true).
    EvaluatingAsync,
    /// Evaluation finished — successfully, or with
    /// `evaluation_error` caching the thrown completion.
    Evaluated,
}

/// Evaluation-phase state of one Cyclic Module Record (§16.2.1.4).
#[derive(Debug, Default)]
pub(crate) struct ModuleRecordState {
    /// `[[Status]]` (evaluation slice).
    pub(crate) status: ModuleStatus,
    /// `[[HasTLA]]` — the module's `<module-init>` is async.
    pub(crate) has_tla: bool,
    /// `[[AsyncEvaluationOrder]]` — `Some` iff `[[AsyncEvaluation]]`
    /// is true; the counter preserves the spec's true-ordering for
    /// AsyncModuleExecutionFulfilled's sorted ancestor gather.
    pub(crate) async_order: Option<u64>,
    /// `[[DFSIndex]]` — visit order on the active evaluation stack.
    pub(crate) dfs_index: Option<u64>,
    /// `[[DFSAncestorIndex]]` — earliest on-stack module reachable
    /// from this one; equal to the module's own DFS index when it is
    /// a strongly-connected-component root (§16.2.1.5 step 14).
    pub(crate) dfs_ancestor_index: Option<u64>,
    /// `[[CycleRoot]]` — root of this module's evaluation SCC, set
    /// when the component is popped off the evaluation stack. Waiters
    /// on any cycle member register on the root so they observe the
    /// whole cycle's settlement.
    pub(crate) cycle_root: Option<Arc<str>>,
    /// `[[PendingAsyncDependencies]]` — direct dependencies still
    /// evaluating async. The module's own body runs when this hits 0.
    pub(crate) pending_async_dependencies: usize,
    /// `[[AsyncParentModules]]` — importers waiting on this module's
    /// async settlement, notified by the fulfilled/rejected walks.
    pub(crate) async_parent_modules: Vec<Arc<str>>,
    /// `[[TopLevelCapability]]`-shaped gate: pending while the module
    /// (or its async subtree) evaluates, settled by
    /// AsyncModuleExecutionFulfilled / Rejected. Present for every
    /// module that evaluates async, not only cycle roots.
    pub(crate) evaluation_promise: Option<JsPromiseHandle>,
    /// `[[EvaluationError]]` — cached thrown completion, rethrown on
    /// every later evaluation request.
    pub(crate) evaluation_error: Option<Value>,
}

impl Interpreter {
    /// Shared record lookup; absent records read as status `New`.
    pub(crate) fn module_record(&self, url: &str) -> Option<&ModuleRecordState> {
        self.module_records.get(url)
    }

    /// Record for `url`, created in status `New` on first touch.
    pub(crate) fn module_record_mut(&mut self, url: &Arc<str>) -> &mut ModuleRecordState {
        self.module_records.entry(url.clone()).or_default()
    }

    pub(crate) fn module_record_status(&self, url: &str) -> ModuleStatus {
        self.module_record(url)
            .map_or(ModuleStatus::New, |r| r.status)
    }
}
