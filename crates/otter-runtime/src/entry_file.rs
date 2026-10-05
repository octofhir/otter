//! File-entry routing and source-owned CommonJS execution.
//!
//! # Contents
//! - `Runtime::run_file_inner` selects script, module or CommonJS semantics.
//! - `Runtime::run_commonjs_file` enters one scoped native load and checkpoint.
//!
//! # Invariants
//! - File/source classification and capability policy retain their existing
//!   owners. Scripts and modules enter their real admitted context only while
//!   executing; a file-entry result retains no executable context.
//! - CommonJS entry starts with no source context. Each wrapper is compiled
//!   and linked once by the normal CommonJS loader, and calls resolve its exact
//!   FunctionID/source owner. No empty bootstrap module is linked for dispatch.
//! - Compiler syntax errors keep their compiler diagnostic; exits keep their
//!   actual code. Other native completions transfer their existing pending
//!   detail/frames through the canonical projector before any uncaught handler.
//!   Fatal or escaping OOM never enters handlers or a checkpoint.
//! - The CommonJS loader's guarded record rollback compares ordinary data
//!   slots without invoking accessors or Proxy traps. The pending throw and
//!   owned completed-failure provenance stay rooted through metadata cleanup.
//!
//! # See also
//! - `crate::commonjs` owns module records, wrappers and the require cache.
//! - `crate::checkpoint` owns completed-failure checkpoint admission.
//! - `crate::worker` owns FIFO entry readiness and message delivery.

use super::*;

impl Runtime {
    pub(crate) fn run_file_inner(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<ExecutionResult, OtterError> {
        let path = path.as_ref();
        let source = SourceInput::from_path(path)?;
        if source_path_has_module_extension(path) {
            return self.run_module_with_context(path).map(|(result, _)| result);
        }
        let package_type = {
            let loader = self.module_loader_for_entry(path);
            source_path_package_type(path, &loader)
        };
        if package_type == Some(module_loader::LoaderPackageType::Module) {
            return self.run_module_with_context(path).map(|(result, _)| result);
        }
        let specifier = path.to_string_lossy().to_string();
        match self.commonjs_routing(path, &source, package_type)? {
            CommonJsRouting::CommonJs => return self.run_commonjs_file(path, source),
            CommonJsRouting::Module => {
                return self.run_module_with_context(path).map(|(result, _)| result);
            }
            CommonJsRouting::Script => {}
        }
        if package_type == Some(module_loader::LoaderPackageType::CommonJs) {
            return self
                .run_script_with_context(source, &specifier)
                .map(|(result, _)| result);
        }
        if !source_path_has_script_extension(path) {
            let start = std::time::Instant::now();
            let module = with_program(&source.text, source.kind, |program| {
                if program_looks_like_module(program) {
                    return Ok(None);
                }
                compile_script_program(program, source.kind, &specifier)
                    .map(Some)
                    .map_err(|err| map_compile_error(err, &specifier))
            })
            .map_err(|err| map_syntax_error(err, &specifier))??;
            if let Some(module) = module {
                let (module, sources) = self.prepare_script_source(module, source, &specifier)?;
                return self
                    .run_compiled_script_with_context_since(module, sources, start)
                    .map(|(result, _)| result);
            }
            return self.run_module_with_context(path).map(|(result, _)| result);
        }
        let specifier = path.to_string_lossy().to_string();
        self.run_script_with_context(source, &specifier)
            .map(|(result, _)| result)
    }

    pub(super) fn run_commonjs_file(
        &mut self,
        path: &Path,
        source: SourceInput,
    ) -> Result<ExecutionResult, OtterError> {
        let start = std::time::Instant::now();
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let cfg = std::sync::Arc::new(commonjs::CjsConfig {
            capabilities: self.config.capabilities.clone(),
            hosted: self.config.hosted_modules.clone(),
            runtime_task_spawner: self.runtime_task_spawner.clone(),
            addon_loader: self.config.commonjs_addon_loader,
            report_watch_dependencies: crate::commonjs::watch_reporting_requested(),
        });
        let load = otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::default_call(),
            None,
            |ctx| {
                // This is one fresh file entry, not nested require re-entry.
                ctx.clear_pending_error();
                commonjs::cjs_instantiate_file(ctx, &cfg, &abs, &source.text).map(|_| ())
            },
        );
        match load {
            Err(error) => {
                if let Some(code) = error.exit_code() {
                    let result = ExecutionResult::from_exit_code(code, start.elapsed());
                    return Ok(self.attach_execution_stats(result));
                }
                let error = match error {
                    otter_vm::NativeError::SyntaxError { reason, .. } => {
                        // The wrapper compiler owns this diagnostic. It is not
                        // a fabricated throw or an internal loader failure.
                        return Err(OtterError::Internal {
                            code: DiagnosticCode::SyntaxError.as_str().to_owned(),
                            message: reason,
                        });
                    }
                    error => error,
                };
                let failure = otter_vm::NativeCtx::with_host_context(
                    &mut self.interp,
                    otter_vm::NativeCallInfo::default_call(),
                    None,
                    |ctx| ctx.take_native_error(error),
                );
                if checkpoint::stops(&failure.error) {
                    return Err(map_vm_error(failure));
                }
                if !self.dispatch_uncaught_exception(None)? {
                    return Err(enrich_runtime_diagnostic_with_cause(
                        &mut self.interp,
                        map_vm_error(failure),
                    ));
                }
            }
            Ok(()) => {}
        }
        if let Err(error) = self.drain_microtasks_dispatching_uncaught() {
            if let otter_vm::VmError::Exit { code } = error.error {
                let result = ExecutionResult::from_exit_code(code, start.elapsed());
                return Ok(self.attach_execution_stats(result));
            }
            return Err(enrich_runtime_diagnostic_with_cause(
                &mut self.interp,
                map_vm_error(error),
            ));
        }
        let result = ExecutionResult::from_vm_value(
            otter_vm::Value::undefined(),
            start.elapsed(),
            self.interp.gc_heap_mut(),
        )
        .with_exit_code(process::exit_code(&self.interp));
        Ok(self.attach_execution_stats(result))
    }
}

#[cfg(test)]
mod tests;
