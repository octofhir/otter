//! Module-related opcode helpers and Cyclic Module Record evaluation.
//!
//! Static namespace imports and `import.meta.resolve` are fixed-width
//! bytecodes, so dispatch can decode their operands from the executable view.
//!
//! # Contents
//! - §16.2.1.4 Evaluate / §16.2.1.5 InnerModuleEvaluation over
//!   interpreter-owned [`crate::module_records`], including the DFS
//!   stack, strongly-connected-component `[[CycleRoot]]` assignment,
//!   and §16.2.1.9 ExecuteAsyncModule +
//!   AsyncModuleExecutionFulfilled/Rejected walks.
//! - Static namespace object resolution.
//! - Dynamic `import(specifier)` promise construction / scheduling.
//! - Deferred namespace force-evaluation (§28.3 ReadyForSyncExecution).
//! - `import.meta.resolve(specifier)` relative URL resolution.
//!
//! # Invariants
//! - All async-walk state lives in the records map — reaction closures
//!   capture only module URLs, never `Rc` state or side-channel
//!   counters.
//! - A top-level-await module parks only its own evaluation gate;
//!   siblings keep evaluating (§16.2.1.5 never awaits a dependency).
//! - Static namespace imports must already be present in the linked module
//!   namespace table.
//! - Dynamic import always writes a Promise to the destination register.
//! - `import.meta.resolve` accepts only string specifiers.
//!
//! # See also
//! - [`crate::module_records`]
//! - [`crate::execution_context`]

use crate::holt_stack::HoltStack;
use crate::{
    ExecutionContext, Frame, Interpreter, JsString, Value, VmError, module_records::ModuleStatus,
    operand_decode::register_operand, promise_dispatch, read_register, resolve_relative_url,
    write_register,
};
use otter_bytecode::Operand;
use smallvec::SmallVec;

/// Per-Evaluate DFS state (§16.2.1.5): the spec's `stack` plus the
/// monotonically increasing `index` handed to each new module visit.
struct ModuleEvalState {
    stack: Vec<std::sync::Arc<str>>,
    next_index: u64,
}

impl Interpreter {
    pub(crate) fn run_import_namespace_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        spec_idx: u32,
    ) -> Result<(), VmError> {
        let specifier = context
            .string_constant_str(spec_idx)
            .ok_or(VmError::InvalidOperand)?;
        let referrer: String = context
            .exec_function(frame.function_id)
            .map(|f| f.module_url.as_ref().to_string())
            .unwrap_or_default();
        let namespace = self
            .resolve_module_namespace(context, referrer.as_str(), specifier)
            .ok_or_else(|| VmError::UnknownIntrinsic {
                name: format!("import \"{specifier}\""),
            })?;
        write_register(frame, dst, Value::object(namespace))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// `Op::ModuleNamespaceObject` — resolve the Module Namespace
    /// Exotic Object (§10.4.6) for `specifier` and write it to `dst`.
    /// Used by `import * as ns` / `export * as ns`; distinct from the
    /// raw module environment yielded by [`Op::ImportNamespace`].
    pub(crate) fn run_module_namespace_object_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        spec_idx: u32,
    ) -> Result<(), VmError> {
        let specifier = context
            .string_constant_str(spec_idx)
            .ok_or(VmError::InvalidOperand)?
            .to_string();
        let referrer: String = context
            .exec_function(frame.function_id)
            .map(|f| f.module_url.as_ref().to_string())
            .unwrap_or_default();
        let namespace = self
            .resolve_module_namespace_object(context, referrer.as_str(), specifier.as_str())
            .ok_or_else(|| VmError::UnknownIntrinsic {
                name: format!("import * as \"{specifier}\""),
            })?;
        write_register(frame, dst, Value::object(namespace))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// `Op::LoadImportBinding` — read named import `name` from the
    /// source module `url` via its §16.2.1.6 ResolveExport table, so a
    /// re-exported / star-exported name reads the *defining* module's
    /// live binding. A slot still in its TDZ (the hole) raises a
    /// `ReferenceError` (§9.1.1.5 GetBindingValue); otherwise the
    /// current binding value is written to `dst`.
    pub(crate) fn run_load_import_binding_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        url_idx: u32,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let url = context
            .string_constant_str(url_idx)
            .ok_or(VmError::InvalidOperand)?
            .to_string();
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?
            .to_string();
        let value = self
            .resolve_module_binding(&url, &name)
            .unwrap_or_else(Value::undefined);
        if value.is_hole() {
            return Err(VmError::ThisUninitialized {
                message: (format!("Cannot access '{name}' before initialization")).into(),
            });
        }
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// `Op::ImportNamespaceDeferred` — resolve (or lazily create) the
    /// deferred namespace object for `specifier` and write it to `dst`.
    /// The target module is **not** evaluated here (TC39 import defer).
    pub(crate) fn run_import_namespace_deferred_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        spec_idx: u32,
    ) -> Result<(), VmError> {
        let specifier = context
            .string_constant_str(spec_idx)
            .ok_or(VmError::InvalidOperand)?
            .to_string();
        let referrer: String = context
            .exec_function(frame.function_id)
            .map(|f| f.module_url.as_ref().to_string())
            .unwrap_or_default();
        let target = context
            .module_resolution_target(referrer.as_str(), specifier.as_str())
            .ok_or_else(|| VmError::UnknownIntrinsic {
                name: format!("import defer \"{specifier}\""),
            })?
            .to_string();
        let target_url: std::sync::Arc<str> = std::sync::Arc::from(target.as_str());
        let ns = self.get_or_create_deferred_namespace(target_url)?;
        write_register(frame, dst, Value::object(ns))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// `Op::EvaluateModule` — evaluate the module named by the constant
    /// operand and its non-deferred dependency closure (idempotent).
    /// Writes the module's evaluation gate promise to `dst` when the
    /// subtree evaluates async, `undefined` when it completed
    /// synchronously — the async `<entry>` driver awaits the register.
    pub(crate) fn run_evaluate_module_const(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        url_idx: u32,
    ) -> Result<(), VmError> {
        let url = context
            .string_constant_str(url_idx)
            .ok_or(VmError::InvalidOperand)?
            .to_string();
        let gate = self.evaluate_module(context, &url)?;
        let value = gate.map_or_else(Value::undefined, Value::promise);
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// Synchronous-consumer view of [`Self::evaluate_module`]: the
    /// deferred-namespace force-eval path and the sync-graph
    /// [`Op::EvaluateModule`] driver, where an async target is a
    /// `TypeError` (§28.3 ReadyForSyncExecution), not a parked promise.
    pub(crate) fn evaluate_module_rec(
        &mut self,
        context: &ExecutionContext,
        url: &str,
    ) -> Result<(), VmError> {
        if self.module_record_status(url) == ModuleStatus::New
            && context
                .module_init_function_id(url)
                .and_then(|fid| context.function(fid))
                .is_some_and(|f| f.is_async)
        {
            return Err(VmError::TypeError {
                message:
                    ("Cannot synchronously evaluate a deferred module that uses top-level await"
                        .to_string())
                    .into(),
            });
        }
        self.evaluate_module(context, url).map(|_| ())
    }

    /// §16.2.1.4 Evaluate over interpreter-owned records.
    ///
    /// - `Ok(None)` — evaluated synchronously, done.
    /// - `Ok(Some(p))` — evaluating async; `p` settles when the
    ///   module's async subtree settles (or is already settled for an
    ///   `Evaluated` module that went through async evaluation).
    ///
    /// A module that already evaluated async resolves through its
    /// `[[CycleRoot]]`, so waiting on any cycle member waits on the
    /// whole cycle's settlement.
    ///
    /// `pub` so the runtime's dynamic-import host loader drives
    /// freshly linked graphs through the same records.
    ///
    /// # Errors
    /// Rethrows the module's cached `[[EvaluationError]]`, or
    /// propagates infrastructure failures from the init body.
    pub fn evaluate_module(
        &mut self,
        context: &ExecutionContext,
        url: &str,
    ) -> Result<Option<crate::promise::JsPromiseHandle>, VmError> {
        // §16.2.1.4 Evaluate step 2 — an evaluating-async / evaluated
        // module is represented by its cycle root.
        let url_arc: std::sync::Arc<str> = match self.module_record(url) {
            Some(record)
                if matches!(
                    record.status,
                    ModuleStatus::EvaluatingAsync | ModuleStatus::Evaluated
                ) =>
            {
                record
                    .cycle_root
                    .clone()
                    .unwrap_or_else(|| std::sync::Arc::from(url))
            }
            _ => std::sync::Arc::from(url),
        };
        if let Some(record) = self.module_record(&url_arc) {
            match record.status {
                ModuleStatus::Evaluated => {
                    if let Some(thrown) = record.evaluation_error {
                        self.set_pending_uncaught_throw(thrown);
                        return Err(VmError::Uncaught {
                            value: (self.render_thrown(&thrown)).into(),
                        });
                    }
                    return Ok(record.evaluation_promise);
                }
                ModuleStatus::EvaluatingAsync => return Ok(record.evaluation_promise),
                // Re-entry while a body on the active stack evaluates
                // (e.g. the entry sweep) — nothing new to drive.
                ModuleStatus::Evaluating => return Ok(None),
                ModuleStatus::New => {}
            }
        }
        // §16.2.1.7 InitializeEnvironment — instantiate hoisted
        // function declarations for every module in the subtree
        // before ANY body evaluates, so cyclic importers observe the
        // exporting module's functions during their own evaluation.
        self.hoist_module_subtree(context, &url_arc)?;
        let mut state = ModuleEvalState {
            stack: Vec::new(),
            next_index: 0,
        };
        self.module_evaluation_depth += 1;
        let evaluation = self.inner_module_evaluation(context, &mut state, &url_arc);
        self.module_evaluation_depth -= 1;
        match evaluation {
            Ok(()) => {
                debug_assert!(state.stack.is_empty());
                Ok(self
                    .module_record(&url_arc)
                    .and_then(|record| record.evaluation_promise))
            }
            Err(err) => Err(self.fail_evaluation_stack(state, err)),
        }
    }

    /// §16.2.1.5 InnerModuleEvaluation. `state.stack` is the spec's
    /// DFS stack; strongly connected components are popped when a
    /// module's `[[DFSAncestorIndex]]` equals its own index, giving
    /// every member its `[[CycleRoot]]`.
    ///
    /// A parked async dependency contributes a pending count and
    /// registers this module in its `[[AsyncParentModules]]` — the
    /// dependency loop never awaits, so a sibling continues evaluating
    /// while a top-level-await module is suspended.
    fn inner_module_evaluation(
        &mut self,
        context: &ExecutionContext,
        state: &mut ModuleEvalState,
        url_arc: &std::sync::Arc<str>,
    ) -> Result<(), VmError> {
        let url = url_arc.as_ref();
        // §16.2.1.5 steps 2–4.
        if let Some(record) = self.module_record(url) {
            match record.status {
                ModuleStatus::Evaluated | ModuleStatus::EvaluatingAsync => {
                    if let Some(thrown) = record.evaluation_error {
                        self.set_pending_uncaught_throw(thrown);
                        return Err(VmError::Uncaught {
                            value: (self.render_thrown(&thrown)).into(),
                        });
                    }
                    return Ok(());
                }
                ModuleStatus::Evaluating => return Ok(()),
                ModuleStatus::New => {}
            }
        }
        // Steps 5–8.
        let dfs_index = state.next_index;
        state.next_index += 1;
        {
            let record = self.module_record_mut(url_arc);
            record.status = ModuleStatus::Evaluating;
            record.dfs_index = Some(dfs_index);
            record.dfs_ancestor_index = Some(dfs_index);
            record.pending_async_dependencies = 0;
        }
        state.stack.push(url_arc.clone());

        // Step 11 — requested modules in source order. An eager
        // request contributes its target; a defer-phase request
        // contributes GatherAsynchronousTransitiveDependencies of its
        // target (import-defer proposal: a top-level-await module
        // cannot be force-evaluated later, so its TLA roots evaluate
        // eagerly, in request position). A dep that parks does NOT
        // block its later siblings: it only bumps this module's
        // pending count and records the parent edge.
        let requests: Vec<(String, bool)> = context
            .module_requests(url)
            .into_iter()
            .map(|(target, deferred)| (target.to_string(), deferred))
            .collect();
        let mut deps: Vec<String> = Vec::new();
        for (target, deferred) in requests {
            if deferred {
                let mut seen: Vec<String> = Vec::new();
                self.gather_async_transitive_deps(context, &target, &mut seen, &mut deps);
            } else if !deps.contains(&target) {
                deps.push(target);
            }
        }
        for dep in deps {
            let dep_arc: std::sync::Arc<str> = std::sync::Arc::from(dep.as_str());
            self.inner_module_evaluation(context, state, &dep_arc)?;
            // Step 11.c — post-recursion bookkeeping against the dep's
            // (possibly cycle-root) record.
            let dep_status = self.module_record_status(&dep);
            let waited_on: std::sync::Arc<str> = if dep_status == ModuleStatus::Evaluating {
                // 11.c.iii — same SCC: fold the dep's ancestor index
                // into ours.
                let dep_anc = self
                    .module_record(&dep)
                    .and_then(|record| record.dfs_ancestor_index)
                    .unwrap_or(u64::MAX);
                let record = self.module_record_mut(url_arc);
                let own = record.dfs_ancestor_index.unwrap_or(u64::MAX);
                record.dfs_ancestor_index = Some(own.min(dep_anc));
                dep_arc
            } else {
                // 11.c.iv — completed component: wait on its root.
                let root = self
                    .module_record(&dep)
                    .and_then(|record| record.cycle_root.clone())
                    .unwrap_or(dep_arc);
                if let Some(thrown) = self
                    .module_record(&root)
                    .and_then(|record| record.evaluation_error)
                {
                    self.set_pending_uncaught_throw(thrown);
                    return Err(VmError::Uncaught {
                        value: (self.render_thrown(&thrown)).into(),
                    });
                }
                root
            };
            // 11.c.v — an async-evaluating dependency (or cycle root)
            // gates this module.
            if self
                .module_record(&waited_on)
                .is_some_and(|record| record.async_order.is_some())
            {
                self.module_record_mut(url_arc).pending_async_dependencies += 1;
                self.module_record_mut(&waited_on)
                    .async_parent_modules
                    .push(url_arc.clone());
            }
        }

        let has_tla = context
            .module_init_function_id(url)
            .and_then(|fid| context.function(fid))
            .is_some_and(|f| f.is_async);
        let pending = self
            .module_record(url)
            .map_or(0, |record| record.pending_async_dependencies);

        if pending > 0 || has_tla {
            // Step 12 — [[AsyncEvaluation]] := true, in evaluation
            // order. The per-module gate promise generalises the
            // spec's capability-on-cycle-root.
            let gate = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_runtime_rooted(self, &[], &[])
                .map_err(VmError::from)?;
            let order = self.next_module_async_order;
            self.next_module_async_order += 1;
            {
                let record = self.module_record_mut(url_arc);
                record.has_tla = has_tla;
                record.async_order = Some(order);
                record.evaluation_promise = Some(gate);
            }
            if pending == 0 {
                // Step 12.c — ExecuteAsyncModule now; with pending
                // deps the init runs later, triggered by the last
                // dep's fulfilled-walk.
                self.execute_async_module(context, url_arc)?;
            }
        } else {
            // Step 13 — synchronous body; status flips at SCC pop.
            self.run_module_body_sync(context, url, url_arc)?;
        }

        // Steps 14–16 — pop the strongly connected component once its
        // root finishes: every member gets this module as
        // `[[CycleRoot]]` and leaves the `Evaluating` status.
        let is_scc_root = self.module_record(url).is_some_and(|record| {
            record.dfs_ancestor_index == record.dfs_index && record.dfs_index.is_some()
        });
        if is_scc_root {
            while let Some(member) = state.stack.pop() {
                let done = member == *url_arc;
                let record = self.module_record_mut(&member);
                record.cycle_root = Some(url_arc.clone());
                record.status = if record.async_order.is_some() {
                    ModuleStatus::EvaluatingAsync
                } else {
                    ModuleStatus::Evaluated
                };
                if done {
                    break;
                }
            }
        }
        Ok(())
    }

    /// §16.2.1.4 Evaluate step 7 — an abrupt inner completion marks
    /// every module still on the DFS stack `Evaluated` with the thrown
    /// value cached as its `[[EvaluationError]]`.
    fn fail_evaluation_stack(&mut self, state: ModuleEvalState, err: VmError) -> VmError {
        if matches!(err, VmError::Uncaught { .. })
            && let Some(thrown) = self.take_pending_uncaught_throw()
        {
            for member in state.stack {
                let gate = {
                    let record = self.module_record_mut(&member);
                    record.status = ModuleStatus::Evaluated;
                    record.evaluation_error = Some(thrown);
                    record.async_order = None;
                    record.evaluation_promise
                };
                if let Some(gate) = gate {
                    let jobs = crate::JsPromise::reject(&gate, &mut self.gc_heap, thrown);
                    for job in jobs.jobs {
                        self.microtasks.enqueue(job);
                    }
                }
            }
            self.set_pending_uncaught_throw(thrown);
            return VmError::Uncaught {
                value: (self.render_thrown(&thrown)).into(),
            };
        }
        // Infrastructure failure — reset so a later attempt can retry.
        for member in state.stack {
            let record = self.module_record_mut(&member);
            record.status = ModuleStatus::New;
            record.dfs_index = None;
            record.dfs_ancestor_index = None;
        }
        err
    }

    /// Import-defer proposal GatherAsynchronousTransitiveDependencies:
    /// collect the not-yet-evaluated top-level-await roots reachable
    /// from a defer-phase request's target. These evaluate eagerly in
    /// the importer's request position; everything else under the
    /// deferred target stays lazy until first namespace access.
    fn gather_async_transitive_deps(
        &self,
        context: &ExecutionContext,
        target: &str,
        seen: &mut Vec<String>,
        result: &mut Vec<String>,
    ) {
        if seen.iter().any(|s| s == target) {
            return;
        }
        seen.push(target.to_string());
        match self.module_record_status(target) {
            ModuleStatus::Evaluating | ModuleStatus::Evaluated => return,
            ModuleStatus::EvaluatingAsync | ModuleStatus::New => {}
        }
        let has_tla = context
            .module_init_function_id(target)
            .and_then(|fid| context.function(fid))
            .is_some_and(|f| f.is_async);
        if has_tla {
            if !result.iter().any(|r| r == target) {
                result.push(target.to_string());
            }
            return;
        }
        let requests: Vec<String> = context
            .module_requests(target)
            .into_iter()
            .map(|(t, _)| t.to_string())
            .collect();
        for request in requests {
            self.gather_async_transitive_deps(context, &request, seen, result);
        }
    }

    /// Run `url`'s `<module-init>` synchronously to completion (the
    /// non-async arm of §16.2.1.5 step 16 / ExecuteModule).
    fn run_module_body_sync(
        &mut self,
        context: &ExecutionContext,
        url: &str,
        url_arc: &std::sync::Arc<str>,
    ) -> Result<(), VmError> {
        let Some(function_id) = context.module_init_function_id(url) else {
            return Ok(());
        };
        let Some(env) = self.module_environments.get(url_arc).copied() else {
            return Ok(());
        };
        // The hoist phase normally ran in the subtree sweep; keep the
        // pairing as a fallback for inits reached outside
        // `evaluate_module` (defensive — cells must exist before the
        // body binds against them).
        self.run_module_hoist_phase(context, url_arc)?;
        let meta = self.build_import_meta(url)?;
        self.run_module_init(context, function_id, Value::object(env), meta)?;
        Ok(())
    }

    /// Walk the static + dynamic request edges from `root` and run
    /// the link-phase (function-hoisting) init pass for every module
    /// not yet hoisted. Idempotent per module per graph generation.
    fn hoist_module_subtree(
        &mut self,
        context: &ExecutionContext,
        root: &std::sync::Arc<str>,
    ) -> Result<(), VmError> {
        let mut stack: Vec<std::sync::Arc<str>> = vec![root.clone()];
        let mut seen: std::collections::HashSet<std::sync::Arc<str>> =
            std::collections::HashSet::new();
        while let Some(url) = stack.pop() {
            if !seen.insert(url.clone()) {
                continue;
            }
            self.run_module_hoist_phase(context, &url)?;
            for (target, _) in context.module_requests(&url) {
                stack.push(std::sync::Arc::from(target));
            }
        }
        Ok(())
    }

    /// Run one module's link-phase init pass (§16.2.1.7
    /// InitializeEnvironment): export TDZ slots + hoisted function
    /// instantiation into the persistent module environment cells.
    fn run_module_hoist_phase(
        &mut self,
        context: &ExecutionContext,
        url_arc: &std::sync::Arc<str>,
    ) -> Result<(), VmError> {
        if self.module_hoisted.contains(url_arc) {
            return Ok(());
        }
        self.module_hoisted.insert(url_arc.clone());
        let url = url_arc.as_ref();
        let Some(function_id) = context.module_init_function_id(url) else {
            return Ok(());
        };
        let Some(env) = self.module_environments.get(url_arc).copied() else {
            return Ok(());
        };
        let meta = self.build_import_meta(url)?;
        self.run_module_init_hoist(context, function_id, Value::object(env), meta)
    }

    /// §16.2.1.9 ExecuteAsyncModule: run a top-level-await module's
    /// init now and route its async completion into the
    /// fulfilled/rejected walks via the init promise's reactions.
    fn execute_async_module(
        &mut self,
        context: &ExecutionContext,
        url_arc: &std::sync::Arc<str>,
    ) -> Result<(), VmError> {
        let url = url_arc.as_ref();
        let Some(function_id) = context.module_init_function_id(url) else {
            self.async_module_execution_fulfilled(context, url_arc);
            return Ok(());
        };
        let Some(env) = self.module_environments.get(url_arc).copied() else {
            self.async_module_execution_fulfilled(context, url_arc);
            return Ok(());
        };
        self.run_module_hoist_phase(context, url_arc)?;
        let meta = self.build_import_meta(url)?;
        match self.run_module_init(context, function_id, Value::object(env), meta) {
            Ok(Some(init_promise)) => {
                self.attach_async_module_reactions(context, url_arc.clone(), init_promise)
            }
            // ExecuteAsyncModule targets are `[[HasTLA]]` modules, so
            // the init is async; stay safe if the body completed
            // without parking.
            Ok(None) => {
                self.async_module_execution_fulfilled(context, url_arc);
                Ok(())
            }
            Err(err) => {
                let reason = self.thrown_value_for_walk(err)?;
                self.async_module_execution_rejected(url_arc, reason);
                Ok(())
            }
        }
    }

    /// Convert a body-evaluation `VmError` into the JS value the
    /// rejected walk propagates. An infrastructure error (no pending
    /// thrown value) is fatal and propagates as `Err` instead.
    fn thrown_value_for_walk(&mut self, err: VmError) -> Result<Value, VmError> {
        if matches!(err, VmError::Uncaught { .. })
            && let Some(thrown) = self.take_pending_uncaught_throw()
        {
            return Ok(thrown);
        }
        Err(err)
    }

    /// Attach §16.2.1.9 step 8 reactions to a top-level-await init's
    /// result promise. All walk state lives in the records map — the
    /// closures capture only the module URL.
    fn attach_async_module_reactions(
        &mut self,
        context: &ExecutionContext,
        url_arc: std::sync::Arc<str>,
        init: crate::promise::JsPromiseHandle,
    ) -> Result<(), VmError> {
        let fulfilled_url = url_arc.clone();
        let on_fulfilled = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "AsyncModuleExecutionFulfilled",
            SmallVec::new(),
            &mut |_visitor| {},
            move |ncx, _args, _captures| {
                let (interp, reaction_context) = ncx.interp_mut_and_context();
                if let Some(reaction_context) = reaction_context {
                    interp.async_module_execution_fulfilled(&reaction_context, &fulfilled_url);
                }
                Ok(Value::undefined())
            },
        )
        .map_err(VmError::from)?;
        let rejected_url = url_arc;
        let on_rejected = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "AsyncModuleExecutionRejected",
            SmallVec::new(),
            &mut |visitor| on_fulfilled.trace_value_slots(visitor),
            move |ncx, args, _captures| {
                let reason = args.first().copied().unwrap_or_else(Value::undefined);
                ncx.interp_mut()
                    .async_module_execution_rejected(&rejected_url, reason);
                Ok(Value::undefined())
            },
        )
        .map_err(VmError::from)?;
        let capability = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_runtime_rooted(self, &[&on_fulfilled, &on_rejected], &[])?;
        let outcome = crate::JsPromise::perform_then_with_context(
            &init,
            &mut self.gc_heap,
            Some(on_fulfilled),
            Some(on_rejected),
            capability,
            Some(context.clone()),
        );
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
    }

    /// §16.2.1.9.4 AsyncModuleExecutionFulfilled: settle the module's
    /// gate, gather the full transitive set of ancestors made ready by
    /// this completion (§16.2.1.9.3 GatherAvailableAncestors), then
    /// execute them sorted by `[[AsyncEvaluationOrder]]`. Gathering
    /// first is what gives siblings their true-order: a grandparent
    /// made ready through one child must not run before the other
    /// child.
    pub(crate) fn async_module_execution_fulfilled(
        &mut self,
        context: &ExecutionContext,
        url_arc: &std::sync::Arc<str>,
    ) {
        {
            let record = self.module_record_mut(url_arc);
            if record.status == ModuleStatus::Evaluated {
                // Already settled (e.g. the rejected walk got here
                // first through another parent edge).
                return;
            }
            record.status = ModuleStatus::Evaluated;
            record.async_order = None;
        }
        self.fulfill_module_gate(url_arc);
        let mut exec_list: Vec<(u64, std::sync::Arc<str>)> = Vec::new();
        self.gather_available_ancestors(url_arc, &mut exec_list);
        exec_list.sort_by_key(|(order, _)| *order);
        for (_, module) in exec_list {
            if self.module_record_status(&module) == ModuleStatus::Evaluated {
                // §16.2.1.9.4 step 9.a — an earlier rejection in this
                // batch already settled it.
                continue;
            }
            let has_tla = self.module_record(&module).is_some_and(|r| r.has_tla);
            if has_tla {
                // §16.2.1.9.4 step 9.b — its own init reactions
                // continue the walk when it settles.
                if let Err(err) = self.execute_async_module(context, &module) {
                    let message = format!("module evaluation failed: {err}");
                    let reason = self
                        .make_type_error_with_stack_roots(&HoltStack::new(), &message)
                        .unwrap_or_else(|_| Value::undefined());
                    self.async_module_execution_rejected(&module, reason);
                }
                continue;
            }
            // §16.2.1.9.4 step 9.c — ExecuteModule; ancestors were
            // already gathered into this exec list, so success only
            // marks the record evaluated and settles its gate.
            match self.run_module_body_sync(context, module.as_ref(), &module) {
                Ok(()) => {
                    {
                        let record = self.module_record_mut(&module);
                        record.status = ModuleStatus::Evaluated;
                        record.async_order = None;
                    }
                    self.fulfill_module_gate(&module);
                }
                Err(err) => match self.thrown_value_for_walk(err) {
                    Ok(reason) => self.async_module_execution_rejected(&module, reason),
                    Err(err) => {
                        let message = format!("module evaluation failed: {err}");
                        let reason = self
                            .make_type_error_with_stack_roots(&HoltStack::new(), &message)
                            .unwrap_or_else(|_| Value::undefined());
                        self.async_module_execution_rejected(&module, reason);
                    }
                },
            }
        }
    }

    /// §16.2.1.9.3 GatherAvailableAncestors: walk
    /// `[[AsyncParentModules]]`, decrement each parent's pending
    /// count, and collect every parent that became ready. Recurses
    /// through ready sync-bodied parents (their completion is implied
    /// by executing them in this batch) but not through `[[HasTLA]]`
    /// parents, whose own init settlement continues the walk.
    fn gather_available_ancestors(
        &mut self,
        url_arc: &std::sync::Arc<str>,
        exec_list: &mut Vec<(u64, std::sync::Arc<str>)>,
    ) {
        let parents = self
            .module_record(url_arc)
            .map(|record| record.async_parent_modules.clone())
            .unwrap_or_default();
        for parent in parents {
            if exec_list.iter().any(|(_, m)| *m == parent) {
                continue;
            }
            let (became_ready, recurse) = {
                let record = self.module_record_mut(&parent);
                if record.status != ModuleStatus::EvaluatingAsync
                    || record.evaluation_error.is_some()
                {
                    continue;
                }
                record.pending_async_dependencies =
                    record.pending_async_dependencies.saturating_sub(1);
                (
                    record.pending_async_dependencies == 0,
                    record.pending_async_dependencies == 0 && !record.has_tla,
                )
            };
            if became_ready {
                let order = self
                    .module_record(&parent)
                    .and_then(|record| record.async_order)
                    .unwrap_or(u64::MAX);
                exec_list.push((order, parent.clone()));
                if recurse {
                    self.gather_available_ancestors(&parent, exec_list);
                }
            }
        }
    }

    /// Fulfil a module's evaluation gate with `undefined`, enqueueing
    /// the reaction jobs.
    fn fulfill_module_gate(&mut self, url_arc: &std::sync::Arc<str>) {
        if let Some(gate) = self
            .module_record(url_arc)
            .and_then(|record| record.evaluation_promise)
        {
            let jobs = crate::JsPromise::fulfill(&gate, &mut self.gc_heap, Value::undefined());
            for job in jobs.jobs {
                self.microtasks.enqueue(job);
            }
        }
    }

    /// §16.2.1.9.5 AsyncModuleExecutionRejected: cache the error,
    /// reject the gate, and propagate up every parent edge.
    pub(crate) fn async_module_execution_rejected(
        &mut self,
        url_arc: &std::sync::Arc<str>,
        reason: Value,
    ) {
        let parents = {
            let record = self.module_record_mut(url_arc);
            if record.status == ModuleStatus::Evaluated {
                return;
            }
            record.status = ModuleStatus::Evaluated;
            record.evaluation_error = Some(reason);
            record.async_order = None;
            std::mem::take(&mut record.async_parent_modules)
        };
        if let Some(gate) = self
            .module_record(url_arc)
            .and_then(|record| record.evaluation_promise)
        {
            let jobs = crate::JsPromise::reject(&gate, &mut self.gc_heap, reason);
            for job in jobs.jobs {
                self.microtasks.enqueue(job);
            }
        }
        for parent in parents {
            self.async_module_execution_rejected(&parent, reason);
        }
    }

    /// Settle `downstream` when the gating module-evaluation promise
    /// settles: a rejection rejects it (§16.2.1.9
    /// AsyncModuleExecutionRejected); fulfilment resolves it with the
    /// namespace registered for `namespace_url`. The gate mirrors the
    /// spec's `[[TopLevelCapability]]` — one promise per import, no
    /// side-channel completion counting. The downstream handle rides
    /// in the reaction callables' `captures` list so the GC can trace
    /// and relocate it.
    /// §13.3.10 ContinueDynamicImport deferral — evaluate `target` in
    /// a host job (microtask) and settle `pending` with the module's
    /// namespace / thrown error. Used when `import()` fires while a
    /// top-level Evaluate DFS is active, so the dynamic target cannot
    /// preempt the deterministic evaluation order.
    fn defer_dynamic_import_evaluation(
        &mut self,
        context: &ExecutionContext,
        pending: crate::promise::JsPromiseHandle,
        target: String,
        referrer: String,
        specifier: String,
    ) -> Result<(), VmError> {
        let pending_value = Value::promise(pending);
        let job = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "dynamicImportDeferredEvaluate",
            smallvec::smallvec![pending_value],
            &mut |visitor| pending_value.trace_value_slots(visitor),
            move |ncx, _args, captures| {
                let (interp, job_context) = ncx.interp_mut_and_context();
                let Some(job_context) = job_context else {
                    return Ok(Value::undefined());
                };
                let Some(pending) = captures.first().and_then(|v| v.as_promise()) else {
                    return Ok(Value::undefined());
                };
                match interp.evaluate_module(&job_context, &target) {
                    Ok(Some(gate)) => {
                        interp
                            .settle_promise_on_module_evaluation(
                                &job_context,
                                pending,
                                gate,
                                std::sync::Arc::from(target.as_str()),
                            )
                            .map_err(|e| {
                                crate::native_function::vm_to_native_error(e, "import()")
                            })?;
                    }
                    Ok(None) => {
                        let namespace = interp
                            .resolve_module_namespace(&job_context, referrer.as_str(), &specifier)
                            .map(Value::object)
                            .unwrap_or_else(Value::undefined);
                        let jobs =
                            crate::JsPromise::fulfill(&pending, &mut interp.gc_heap, namespace);
                        for j in jobs.jobs {
                            interp.microtasks.enqueue(j);
                        }
                    }
                    Err(err) => {
                        let reason = match err {
                            VmError::Uncaught { .. } => interp
                                .take_pending_uncaught_throw()
                                .unwrap_or_else(Value::undefined),
                            other => {
                                return Err(crate::native_function::vm_to_native_error(
                                    other, "import()",
                                ));
                            }
                        };
                        let jobs = crate::JsPromise::reject(&pending, &mut interp.gc_heap, reason);
                        for j in jobs.jobs {
                            interp.microtasks.enqueue(j);
                        }
                    }
                }
                Ok(Value::undefined())
            },
        )
        .map_err(VmError::from)?;
        self.microtasks.enqueue(crate::microtask::Microtask {
            callee: job,
            this_value: Value::undefined(),
            args: smallvec::SmallVec::new(),
            context: Some(context.clone()),
            result_capability: None,
            kind: crate::microtask::MicrotaskKind::Call,
        });
        Ok(())
    }

    pub(crate) fn settle_promise_on_module_evaluation(
        &mut self,
        context: &ExecutionContext,
        downstream: crate::promise::JsPromiseHandle,
        init: crate::promise::JsPromiseHandle,
        namespace_url: std::sync::Arc<str>,
    ) -> Result<(), VmError> {
        let downstream_value = Value::promise(downstream);
        let on_fulfilled = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "dynamicImportModuleFulfilled",
            smallvec::smallvec![downstream_value],
            &mut |visitor| downstream_value.trace_value_slots(visitor),
            move |ncx, _args, captures| {
                let interp = ncx.interp_mut();
                if let Some(downstream) = captures.first().and_then(|v| v.as_promise()) {
                    let namespace = interp
                        .module_env(&namespace_url)
                        .map(Value::object)
                        .unwrap_or_else(Value::undefined);
                    let jobs =
                        crate::JsPromise::fulfill(&downstream, &mut interp.gc_heap, namespace);
                    for j in jobs.jobs {
                        interp.microtasks.enqueue(j);
                    }
                }
                Ok(Value::undefined())
            },
        )
        .map_err(VmError::from)?;
        let on_rejected = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "dynamicImportModuleRejected",
            smallvec::smallvec![downstream_value],
            &mut |visitor| {
                downstream_value.trace_value_slots(visitor);
                on_fulfilled.trace_value_slots(visitor);
            },
            move |ncx, args, captures| {
                let interp = ncx.interp_mut();
                if let Some(downstream) = captures.first().and_then(|v| v.as_promise()) {
                    let reason = args.first().copied().unwrap_or_else(Value::undefined);
                    let jobs = crate::JsPromise::reject(&downstream, &mut interp.gc_heap, reason);
                    for j in jobs.jobs {
                        interp.microtasks.enqueue(j);
                    }
                }
                Ok(Value::undefined())
            },
        )
        .map_err(VmError::from)?;
        let capability = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_runtime_rooted(self, &[&on_fulfilled, &on_rejected], &[])?;
        let outcome = crate::JsPromise::perform_then_with_context(
            &init,
            &mut self.gc_heap,
            Some(on_fulfilled),
            Some(on_rejected),
            capability,
            Some(context.clone()),
        );
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
    }

    /// Build a module's `import.meta` object with its `url` property.
    fn build_import_meta(&mut self, url: &str) -> Result<Value, VmError> {
        let obj = self.alloc_host_object_with_roots(&[], &[])?;
        let url_str =
            JsString::from_str(url, &mut self.gc_heap).map_err(|_| VmError::TypeMismatch)?;
        crate::object::set(obj, &mut self.gc_heap, "url", Value::string(url_str));
        Ok(Value::object(obj))
    }

    /// Deferred namespace object for `target_url`, created on first use
    /// and cached so repeated `import defer` of the same module yield
    /// the identical object.
    pub(crate) fn get_or_create_deferred_namespace(
        &mut self,
        target_url: std::sync::Arc<str>,
    ) -> Result<crate::object::JsObject, VmError> {
        if let Some(obj) = self.deferred_namespaces.get(&target_url) {
            return Ok(*obj);
        }
        let obj = self.alloc_deferred_namespace_object(target_url.clone())?;
        self.deferred_namespaces.insert(target_url, obj);
        Ok(obj)
    }

    /// Ensure a deferred module namespace is ready for an operation.
    ///
    /// When `value` is a deferred namespace and `trigger` is `true`, the
    /// wrapped module is evaluated and its export data properties are
    /// installed (idempotent). `trigger` is `false` for symbol-like keys
    /// (symbols and `"then"`), which §28.3 reads without forcing
    /// evaluation. A no-op for ordinary values, so callers fall through
    /// to normal handling on the same object afterward.
    pub(crate) fn ensure_deferred_namespace_ready(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
        trigger: bool,
    ) -> Result<(), VmError> {
        let Some(obj) = value.as_object() else {
            return Ok(());
        };
        let Some(target_url) = crate::object::deferred_namespace_target(obj, &self.gc_heap) else {
            return Ok(());
        };
        if !trigger || crate::object::deferred_namespace_is_populated(obj, &self.gc_heap) {
            return Ok(());
        }
        if !self.module_ready_for_sync_execution(context, target_url.as_ref(), &mut Vec::new()) {
            return Err(VmError::TypeError {
                message: ("Cannot synchronously evaluate a deferred module while it is evaluating"
                    .to_string())
                .into(),
            });
        }
        self.evaluate_module_rec(context, &target_url)?;
        if let Some(env) = self.module_environments.get(&target_url).copied() {
            let env_value = Value::object(env);
            let mut names = self.enumerable_own_string_keys_for_value(context, env_value, 0)?;
            names.sort();
            for name in &names {
                let key = crate::VmPropertyKey::String(name);
                let val = match self.ordinary_get_value(context, env_value, env_value, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        self.run_callable_sync(context, &getter, env_value, SmallVec::new())?
                    }
                };
                // §28.3 namespace export properties: writable, enumerable,
                // non-configurable.
                crate::object::define_own_property(
                    obj,
                    &mut self.gc_heap,
                    name,
                    crate::object::PropertyDescriptor::data(val, true, true, false),
                );
            }
            crate::object::prevent_extensions(obj, &mut self.gc_heap);
        }
        crate::object::set_deferred_namespace_populated(obj, &self.gc_heap);
        Ok(())
    }

    /// Conservative §28.3 ReadyForSyncExecution over the active
    /// non-deferred dependency graph.
    fn module_ready_for_sync_execution(
        &self,
        context: &ExecutionContext,
        url: &str,
        seen: &mut Vec<std::sync::Arc<str>>,
    ) -> bool {
        let url_arc: std::sync::Arc<str> = std::sync::Arc::from(url);
        if seen.iter().any(|seen| seen.as_ref() == url) {
            return true;
        }
        seen.push(url_arc);
        match self.module_record_status(url) {
            ModuleStatus::Evaluated => return true,
            ModuleStatus::Evaluating | ModuleStatus::EvaluatingAsync => return false,
            ModuleStatus::New => {}
        }
        if context
            .module_init_function_id(url)
            .and_then(|fid| context.function(fid))
            .is_some_and(|f| f.is_async)
        {
            return false;
        }
        context
            .eager_dep_targets(url)
            .into_iter()
            .all(|dep| self.module_ready_for_sync_execution(context, dep, seen))
    }

    /// `true` when `key` is symbol-like for a deferred namespace: a
    /// Symbol, or the string `"then"` (§28.3 IsSymbolLikeNamespaceKey).
    /// Reads of such keys do not trigger module evaluation.
    pub(crate) fn deferred_key_is_symbol_like(key: &crate::VmPropertyKey<'_>) -> bool {
        match key.string_name() {
            Some(name) => name == "then",
            None => true,
        }
    }

    pub(crate) fn run_import_meta_resolve_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        spec_reg: u16,
    ) -> Result<(), VmError> {
        let spec_value = *read_register(frame, spec_reg)?;
        let specifier = spec_value
            .as_string(&self.gc_heap)
            .ok_or(VmError::TypeMismatch)?
            .to_lossy_string(&self.gc_heap);
        let referrer: Option<&str> = context
            .exec_function(frame.function_id)
            .map(|f| f.module_url.as_ref());
        let resolved = resolve_relative_url(referrer, &specifier);
        let resolved_str =
            JsString::from_str(&resolved, &mut self.gc_heap).map_err(|_| VmError::TypeMismatch)?;
        write_register(frame, dst, Value::string(resolved_str))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_import_namespace_dynamic_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let spec_reg = register_operand(operands.get(1))?;
        let spec_value = *read_register(&stack[top_idx], spec_reg)?;
        let referrer: String = context
            .exec_function(stack[top_idx].function_id)
            .map(|f| f.module_url.as_ref().to_string())
            .unwrap_or_default();
        let import_context = context.clone();
        let promise = match self.coerce_to_string(context, &spec_value) {
            Ok(specifier) => {
                if let Some(target) =
                    context.module_resolution_target(referrer.as_str(), &specifier)
                {
                    let target = target.to_string();
                    // §16.2.1.4 Evaluate step 1 / §13.3.10 — a dynamic
                    // import during an active Evaluate DFS must not
                    // preempt it; defer the target's evaluation to a
                    // host job.
                    if self.module_evaluation_depth > 0
                        && self.module_record_status(&target) == ModuleStatus::New
                    {
                        let pending =
                            promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                                .pending_stack_rooted(self, stack, &[], &[])?;
                        self.defer_dynamic_import_evaluation(
                            context,
                            pending,
                            target,
                            referrer.clone(),
                            specifier.clone(),
                        )?;
                        let frame = &mut stack[top_idx];
                        write_register(frame, dst, Value::promise(pending))?;
                        frame.advance_pc(self.current_byte_len)?;
                        return Ok(());
                    }
                    match self.evaluate_module(context, &target) {
                        Ok(Some(gate)) => {
                            // §13.3.10 — an async-evaluating target
                            // settles the import promise only when its
                            // module subtree does; the per-record gate
                            // is the import's `[[TopLevelCapability]]`.
                            let pending = promise_dispatch::PromiseBuilder::with_context(
                                import_context.clone(),
                            )
                            .pending_stack_rooted(
                                self,
                                stack,
                                &[],
                                &[],
                            )?;
                            self.settle_promise_on_module_evaluation(
                                context,
                                pending,
                                gate,
                                std::sync::Arc::from(target.as_str()),
                            )?;
                            pending
                        }
                        Ok(None) => {
                            let ns = self
                                .resolve_module_namespace(context, referrer.as_str(), &specifier)
                                .ok_or_else(|| VmError::UnknownIntrinsic {
                                    name: format!("import \"{specifier}\""),
                                })?;
                            let namespace_value = Value::object(ns);
                            promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                                .fulfilled_stack_rooted(self, stack, namespace_value, &[], &[])?
                        }
                        Err(VmError::Uncaught { .. }) => {
                            let reason = self
                                .take_pending_uncaught_throw()
                                .unwrap_or_else(Value::undefined);
                            promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                                .rejected_stack_rooted(self, stack, reason, &[], &[])?
                        }
                        Err(err) => {
                            let reason = self.make_type_error_with_stack_roots(
                                stack,
                                &format!("dynamic import: evaluation failed: {err}"),
                            )?;
                            promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                                .rejected_stack_rooted(self, stack, reason, &[], &[])?
                        }
                    }
                } else if let Some(loader) = self.dynamic_import_loader.clone() {
                    let pending =
                        promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                            .pending_stack_rooted(self, stack, &[], &[])?;
                    let token = self
                        .dynamic_import_registry
                        .insert(pending, import_context.clone());
                    self.record_runtime_host_op_enqueued();
                    loader.schedule(token, specifier, referrer.clone());
                    pending
                } else {
                    let reason = self.make_type_error_with_stack_roots(
                        stack,
                        &format!("dynamic import: module not resolvable: \"{specifier}\""),
                    )?;
                    promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                        .rejected_stack_rooted(self, stack, reason, &[], &[])?
                }
            }
            Err(VmError::Uncaught { value }) => {
                let reason = if let Some(thrown) = self.take_pending_uncaught_throw() {
                    thrown
                } else {
                    let fallback = JsString::from_str(&value, &mut self.gc_heap).map_err(|_| {
                        VmError::TypeError {
                            message: ("dynamic import: failed to allocate rejection reason"
                                .to_string())
                            .into(),
                        }
                    })?;
                    Value::string(fallback)
                };
                promise_dispatch::PromiseBuilder::with_context(import_context)
                    .rejected_stack_rooted(self, stack, reason, &[], &[])?
            }
            Err(err) => {
                let reason = self.make_type_error_with_stack_roots(
                    stack,
                    &format!("dynamic import: specifier ToString failed: {err}"),
                )?;
                promise_dispatch::PromiseBuilder::with_context(import_context)
                    .rejected_stack_rooted(self, stack, reason, &[], &[])?
            }
        };
        write_register(&mut stack[top_idx], dst, Value::promise(promise))?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        Ok(())
    }
}
