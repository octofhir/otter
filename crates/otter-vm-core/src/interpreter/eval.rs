use super::*;
use crate::context::DispatchAction;

impl Interpreter {
    pub(crate) fn execute_eval_module(
        &self,
        ctx: &mut VmContext,
        module: &Module,
    ) -> VmResult<Value> {
        let module = Arc::new(module.clone());
        let entry_func = module
            .entry_function()
            .ok_or_else(|| VmError::internal("eval: no entry function"))?;

        let prev_stack_depth = ctx.stack_depth();

        ctx.register_module(&module);
        ctx.push_frame(
            module.entry_point,
            module.module_id,
            entry_func.local_count,
            None,
            false,
            entry_func.is_async(),
            0,
        )?;

        // Mini run-loop that returns when eval frame completes
        loop {
            if ctx.should_check_interrupt() {
                ctx.update_debug_snapshot();
                if ctx.is_interrupted() {
                    // Pop the eval frame before returning error
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame_discard();
                    }
                    return Err(VmError::interrupted());
                }
                ctx.update_debug_snapshot();
            }

            let frame = ctx
                .current_frame()
                .ok_or_else(|| VmError::internal("eval: no frame"))?;
            let current_module = Arc::clone(ctx.module_table.get(frame.module_id));
            let eval_func_index = frame.function_index;
            let func = current_module
                .function(eval_func_index)
                .ok_or_else(|| VmError::internal("eval: function not found"))?;

            // End of function → implicit return undefined
            if frame.pc >= func.instructions.read().len() {
                if ctx.stack_depth() <= prev_stack_depth {
                    return Ok(Value::undefined());
                }
                ctx.pop_frame_discard();
                continue;
            }

            let instruction = &func.instructions.read()[frame.pc];
            ctx.record_instruction();

            let exec_result = self.execute_instruction(instruction, &current_module, ctx);

            // Convert typed errors to Throw dispatch actions
            match exec_result {
                Ok(()) => {}
                Err(VmError::Exception(thrown)) => {
                    ctx.dispatch_action = Some(DispatchAction::Throw(thrown.value));
                }
                Err(VmError::SyntaxError(msg)) => {
                    let error_val = self.make_error(ctx, "SyntaxError", &msg);
                    ctx.dispatch_action = Some(DispatchAction::Throw(error_val));
                }
                Err(VmError::TypeError(msg)) => {
                    let error_val = self.make_error(ctx, "TypeError", &msg);
                    ctx.dispatch_action = Some(DispatchAction::Throw(error_val));
                }
                Err(VmError::ReferenceError(msg)) => {
                    let error_val = self.make_error(ctx, "ReferenceError", &msg);
                    ctx.dispatch_action = Some(DispatchAction::Throw(error_val));
                }
                Err(VmError::RangeError(msg)) => {
                    let error_val = self.make_error(ctx, "RangeError", &msg);
                    ctx.dispatch_action = Some(DispatchAction::Throw(error_val));
                }
                Err(other) => {
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame_discard();
                    }
                    return Err(other);
                }
            }

            if let Some(action) = ctx.take_dispatch_action() {
                match action {
                    DispatchAction::Jump(offset) => {
                        if offset < 0 {
                            let newly_hot = func.record_back_edge();
                            if newly_hot {
                                func.mark_hot();
                            }
                        }
                        ctx.jump(offset);
                    }
                    DispatchAction::Return(value) => {
                        if ctx.stack_depth() <= prev_stack_depth + 1 {
                            // Capture exports before popping the entry frame
                            self.capture_module_exports(ctx, &module);
                            ctx.pop_frame_discard();
                            return Ok(value);
                        }
                        let return_reg = ctx
                            .current_frame()
                            .and_then(|f| f.return_register)
                            .unwrap_or(0);
                        ctx.pop_frame_discard();
                        ctx.set_register(return_reg, value);
                    }
                    DispatchAction::Call {
                        func_index,
                        module_id,
                        argc,
                        return_reg,
                        is_construct,
                        is_async,
                        upvalues,
                    } => {
                        ctx.advance_pc();
                        let local_count = {
                            let m = ctx.module_table.get(module_id);
                            m.function(func_index)
                                .ok_or_else(|| {
                                    VmError::internal("eval: called function not found")
                                })?
                                .local_count
                        };
                        ctx.push_frame(
                            func_index,
                            module_id,
                            local_count,
                            Some(return_reg),
                            is_construct,
                            is_async,
                            argc as u16,
                        )?;
                        // Set upvalues on the new frame
                        if !upvalues.is_empty()
                            && let Some(frame) = ctx.current_frame_mut()
                        {
                            frame.upvalues = upvalues;
                        }
                    }
                    DispatchAction::Throw(value) => {
                        // Check if there's a try handler within the eval scope
                        if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try()
                            && target_depth > prev_stack_depth
                        {
                            // Handler is within eval scope — use it
                            ctx.take_nearest_try();
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame_discard();
                            }
                            if let Some(frame) = ctx.current_frame_mut() {
                                frame.pc = catch_pc;
                            }
                            ctx.set_exception(value);
                            continue;
                        }
                        // No handler in eval scope — unwind and propagate to outer
                        while ctx.stack_depth() > prev_stack_depth {
                            ctx.pop_frame_discard();
                        }
                        return Err(VmError::exception(value));
                    }
                    DispatchAction::TailCall { .. } => {
                        return Err(VmError::internal("tail call in eval not yet supported"));
                    }
                    DispatchAction::Suspend { .. } => {
                        return Err(VmError::internal("await in eval not yet supported"));
                    }
                    DispatchAction::Yield { .. } => {
                        return Err(VmError::internal("yield in eval not yet supported"));
                    }
                }
            } else {
                ctx.advance_pc();
            }
        }
    }

    /// Capture module exports from the current frame into `ctx.captured_exports`.
    ///
    /// Must be called while the entry frame is still on the stack (before pop_frame).
    /// Mirrors the export capture logic in `execute()`.
    pub(super) fn capture_module_exports(&self, ctx: &mut VmContext, module: &Arc<Module>) {
        let entry_func = match module.entry_function() {
            Some(f) => f,
            None => return,
        };

        let mut exports = std::collections::HashMap::new();
        for export in &module.exports {
            match export {
                otter_vm_bytecode::module::ExportRecord::Named { local, exported } => {
                    if let Some(idx) = entry_func.local_names.iter().position(|n| n == local)
                        && let Ok(val) = ctx.get_local(idx as u16)
                    {
                        exports.insert(exported.clone(), val);
                    }
                }
                otter_vm_bytecode::module::ExportRecord::Default { local } => {
                    if let Some(idx) = entry_func.local_names.iter().position(|n| n == local)
                        && let Ok(val) = ctx.get_local(idx as u16)
                    {
                        exports.insert("default".to_string(), val);
                    }
                }
                _ => {}
            }
        }

        ctx.set_captured_exports(exports);
    }

    pub(super) fn inject_eval_bindings(&self, ctx: &mut VmContext) -> Vec<(PropertyKey, u16)> {
        let mut injected = Vec::new();
        let Some(frame) = ctx.current_frame() else {
            return injected;
        };
        let frame_module_id = frame.module_id;
        let frame_func_index = frame.function_index;
        let frame_module = ctx.module_table.get(frame_module_id);
        let Some(func) = frame_module.function(frame_func_index) else {
            return injected;
        };

        let local_names = func.local_names.clone();
        let global = ctx.global();

        for (index, name) in local_names.iter().enumerate() {
            if name.is_empty() || name.starts_with('$') {
                continue;
            }

            let key = PropertyKey::string(name);
            if matches!(key, PropertyKey::Index(_) | PropertyKey::Symbol(_)) {
                continue;
            }

            if global.has_own(&key) {
                continue;
            }

            let Ok(value) = ctx.get_local(index as u16) else {
                continue;
            };
            if global.set(key, value).is_ok() {
                injected.push((key, index as u16));
            }
        }

        injected
    }

    /// Clean up after direct eval execution:
    /// 1. Remove temporary global properties created during eval (for function-scope eval)
    /// 2. Sync modified values back to outer function locals
    /// 3. Remove injected local→global mirrors
    pub(super) fn cleanup_eval_bindings(
        &self,
        ctx: &mut VmContext,
        injected: &[(PropertyKey, u16)],
        global_keys_before: Option<&[PropertyKey]>,
    ) {
        let global = ctx.global();

        // For function-scope eval: remove any new global properties that were created
        // during eval execution (e.g. block-level function var bindings). These should
        // be scoped to the function's varEnv, not the global.
        if let Some(before_keys) = global_keys_before {
            let before_set: rustc_hash::FxHashSet<PropertyKey> =
                before_keys.iter().copied().collect();
            let current_keys = global.own_keys();
            for key in &current_keys {
                if !before_set.contains(key) {
                    let _ = global.delete(key);
                }
            }
        }

        // Sync modified values back to outer function locals, then remove from global
        for (key, local_idx) in injected {
            if let Some(PropertyDescriptor::Data { value, .. }) =
                global.get_own_property_descriptor(key)
            {
                let _ = ctx.set_local(*local_idx, value);
            }
            let _ = global.delete(key);
        }
    }
}
