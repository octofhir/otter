//! Rooted process lifecycle delivery without synthesized Script chunks.
//!
//! # Contents
//! - `Runtime::emit_process_before_exit` delivers one idle notification.
//! - `Runtime::emit_process_exit` invokes the current exit hook once.
//! - The existing checkpoint and owned completion/error boundary.
//!
//! # Invariants
//! Every syntactic global/member read of the lifecycle expression remains a
//! separate observable operation: a getter may replace the process receiver or
//! callee between the typeof guard and the call. Script lexical bindings and
//! TDZ use the VM's one named-global lookup. All receivers, callees, arguments
//! and successful completions remain collector-rewritten through callbacks and
//! the checkpoint. Bytecode callbacks resolve their own immutable source by
//! FunctionID; native-only delivery invents no dispatch context. The idle loop,
//! exit override, timer cancellation and current diagnostics capture remain
//! owned by the isolate runner.
//!
//! # See also
//! - `crate::handle` owns process liveness and finalization.
//! - `crate::checkpoint` owns fatal/escaping-OOM checkpoint admission.
//! - `otter_vm::NativeScope::global_binding` owns global-environment reads.

use std::time::Instant;

use otter_bytecode::TypeOfKind;
use otter_vm::{NativeCallInfo, NativeCtx, NativeError, NativeScope, Value};

use crate::{ExecutionResult, OtterError, Runtime, checkpoint, process};

#[derive(Clone, Copy)]
enum Lifecycle {
    BeforeExit,
    Exit { code: u8, from_failure: bool },
}

impl Runtime {
    pub(crate) fn emit_process_before_exit(&mut self) -> Result<ExecutionResult, OtterError> {
        self.deliver_process_lifecycle(Lifecycle::BeforeExit)
    }

    pub(crate) fn emit_process_exit(
        &mut self,
        code: u8,
        from_failure: bool,
    ) -> Result<ExecutionResult, OtterError> {
        self.deliver_process_lifecycle(Lifecycle::Exit { code, from_failure })
    }

    fn deliver_process_lifecycle(
        &mut self,
        event: Lifecycle,
    ) -> Result<ExecutionResult, OtterError> {
        let started = Instant::now();
        let outcome = NativeCtx::with_host_context(
            &mut self.interp,
            NativeCallInfo::default_call(),
            None,
            |ctx| {
                ctx.clear_pending_error();
                match ctx.scope(|scope| deliver(scope, event)) {
                    Ok(value) => Ok(ctx.persistent_root_insert(value)),
                    Err(error) => Err(ctx.take_native_error(error)),
                }
            },
        );
        let completion_root = outcome.as_ref().ok().copied();
        let drain =
            checkpoint::after_script(&outcome, || self.drain_microtasks_dispatching_uncaught());
        match (outcome, drain) {
            (Err(error), _) | (Ok(_), Err(error)) => {
                if let Some(root) = completion_root {
                    self.interp.persistent_root_remove(root);
                }
                if let otter_vm::VmError::Exit { code } = error.error {
                    return Ok(ExecutionResult::from_exit_code(code, started.elapsed()));
                }
                Err(crate::enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    crate::map_vm_error(error),
                ))
            }
            (Ok(_), Ok(())) => {
                if let Err(error) = self.pump_layer_a_dynamic_imports() {
                    if let Some(root) = completion_root {
                        self.interp.persistent_root_remove(root);
                    }
                    return Err(error);
                }
                let value = completion_root
                    .and_then(|root| self.interp.persistent_root_remove(root))
                    .expect("lifecycle completion root outlives its checkpoint and import pump");
                // Rendering performs no VM allocation after removing the root.
                Ok(
                    ExecutionResult::from_vm_value(value, started.elapsed(), self.interp.gc_heap())
                        .with_exit_code(process::exit_code(&self.interp)),
                )
            }
        }
    }
}

fn deliver(mut scope: NativeScope<'_, '_>, event: Lifecycle) -> Result<Value, NativeError> {
    if scope.global_binding_typeof("process")? != TypeOfKind::Object {
        return Ok(Value::number_i32(match event {
            Lifecycle::BeforeExit => 0,
            Lifecycle::Exit { code, .. } => i32::from(code),
        }));
    }
    // A fresh binding read is observable independently of the typeof read.
    // Null passes the first guard and its member Get must still throw.
    let process = scope.global_binding("process")?;
    let method = match event {
        Lifecycle::BeforeExit => "emit",
        Lifecycle::Exit { .. } => "__otterEmitExit",
    };
    let guard = scope.get(process, method)?;
    if scope.typeof_kind(guard) != TypeOfKind::Function {
        return Ok(Value::number_i32(match event {
            Lifecycle::BeforeExit => 0,
            Lifecycle::Exit { code, .. } => i32::from(code),
        }));
    }
    let receiver = scope.global_binding("process")?;
    let callee = scope.get(receiver, method)?;
    match event {
        Lifecycle::BeforeExit => {
            let event_name = scope.string("beforeExit")?;
            let process = scope.global_binding("process")?;
            let code_probe = scope.get(process, "exitCode")?;
            let code = if scope.typeof_kind(code_probe) == TypeOfKind::Number {
                let process = scope.global_binding("process")?;
                scope.get(process, "exitCode")?
            } else {
                scope.number(0.0)
            };
            scope.call(callee, receiver, &[event_name, code])?;
            Ok(Value::number_i32(0))
        }
        Lifecycle::Exit { code, from_failure } => {
            let code = scope.number(f64::from(code));
            let from_failure = scope.boolean(from_failure);
            let result = scope.call(callee, receiver, &[code, from_failure])?;
            // No VM allocation follows extracting this collector-current value;
            // the outer callback immediately publishes its persistent root.
            Ok(scope.finish(result))
        }
    }
}

#[cfg(test)]
mod tests;
