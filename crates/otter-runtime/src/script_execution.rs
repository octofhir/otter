//! Classic script compilation and execution.
//!
//! # Contents
//! Classic scripts, linked bytecode, bootstrap modules and checkpoint handling.
//!
//! # Invariants
//! Completion is rooted through the microtask checkpoint.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// Compile and execute `source` as a script.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_script(
        &mut self,
        source: SourceInput,
        specifier: &str,
    ) -> Result<ExecutionResult, OtterError> {
        self.interp.begin_jit_debug_capture();
        self.run_script_with_context(source, specifier)
            .map(|(result, _)| self.attach_jit_debug_report(result))
    }

    /// Run a script while retaining partial JIT diagnostics on abrupt failure.
    pub fn run_script_with_diagnostics(
        &mut self,
        source: SourceInput,
        specifier: &str,
    ) -> ExecutionAttempt {
        let result = self.run_script(source, specifier);
        self.finish_jit_debug_attempt(result)
    }

    /// Run a classic script and return the execution context used for later
    /// host-driven events in the same page/realm.
    ///
    /// Browser embedders retain the owned [`RuntimeExecutionContext`] beside
    /// the document and pass it to [`Self::run_native_event`] for pointer,
    /// keyboard, storage, and broadcast tasks. The context contains no borrow
    /// of the runtime; all actual VM access still requires `&mut Runtime` on
    /// the isolate's owning thread.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_script_with_context(
        &mut self,
        source: SourceInput,
        specifier: &str,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        self.with_direct_timeout(move |runtime| {
            runtime.run_script_with_context_unbounded(source, specifier)
        })
    }

    pub(super) fn run_script_with_context_unbounded(
        &mut self,
        source: SourceInput,
        specifier: &str,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let start = std::time::Instant::now();
        let compiled = self.compile_source(&source, specifier)?;
        let (module, sources) = self.prepare_script_source(compiled.bytecode, source, specifier)?;
        self.run_compiled_script_with_context_since(module, sources, start)
    }

    pub(super) fn run_compiled_script_with_context_since(
        &mut self,
        module: BytecodeModule,
        sources: otter_vm::source_registry::SourceRegistry,
        start: std::time::Instant,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let context = self.interp.link_module(module, sources)?;
        self.run_linked_script_with_context_since(context, start)
    }

    pub(super) fn run_linked_script_with_context_since(
        &mut self,
        context: ExecutionContext,
        start: std::time::Instant,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let (result, context) = self.run_linked_script_outcome(context, start)?;
        Ok((self.attach_execution_stats(result), context))
    }

    /// Run one bootstrap module. Its result is never reported, so it carries
    /// no execution statistics: gathering them walks every linked body.
    pub(super) fn run_bootstrap_module(
        &mut self,
        module: Result<otter_bytecode::VerifiedBytecodeModule, BytecodeModule>,
        sources: otter_vm::source_registry::SourceRegistry,
        start: std::time::Instant,
    ) -> Result<ExecutionResult, OtterError> {
        let context = match module {
            Ok(verified) => self.interp.link_verified_module(verified, sources)?,
            Err(module) => self.interp.link_module(module, sources)?,
        };
        self.run_linked_script_outcome(context, start)
            .map(|(result, _context)| result)
    }

    pub(super) fn run_linked_script_outcome(
        &mut self,
        context: ExecutionContext,
        start: std::time::Instant,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        // Catchable script failure still reaches the checkpoint. A structural
        // or control failure returns unchanged without executing queued work.
        let script_outcome = self.interp.run(&context);
        // The checkpoint and the import pump both allocate in the moving young
        // generation, so the completion cannot wait in a plain local.
        let completion_root = script_outcome
            .as_ref()
            .ok()
            .map(|value| self.interp.persistent_root_insert(*value));
        let drain_outcome = checkpoint::after_script(&script_outcome, || {
            self.drain_microtasks_dispatching_uncaught()
        });
        let release_root = |interp: &mut otter_vm::Interpreter| {
            completion_root.and_then(|root| interp.persistent_root_remove(root))
        };
        match (script_outcome, drain_outcome) {
            (
                Err(otter_vm::RunError {
                    error: otter_vm::VmError::Exit { code },
                    ..
                }),
                _,
            )
            | (
                Ok(_),
                Err(otter_vm::RunError {
                    error: otter_vm::VmError::Exit { code },
                    ..
                }),
            ) => {
                release_root(&mut self.interp);
                let result = ExecutionResult::from_exit_code(code, start.elapsed());
                return Ok((result, context));
            }
            (Err(script_err), _) => {
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(script_err),
                ));
            }
            (Ok(_), Err(drain_err)) => {
                release_root(&mut self.interp);
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(drain_err),
                ));
            }
            (Ok(_), Ok(())) => {}
        }
        if let Err(error) = self.pump_layer_a_dynamic_imports() {
            release_root(&mut self.interp);
            return Err(error);
        }
        let value =
            release_root(&mut self.interp).expect("the completion root outlives the checkpoint");
        let result =
            ExecutionResult::from_vm_value(value, start.elapsed(), self.interp.gc_heap_mut())
                .with_exit_code(process::exit_code(&self.interp));
        Ok((result, context))
    }
}
