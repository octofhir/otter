//! Loop, switch, try/catch/finally, labeled, break/continue statement compilation.
//! Handles finally-scope unwinding, per-iteration lexical bindings for
//! `for`/`for-in`/`for-of`, JumpRecord-based break/continue through finally blocks,
//! and iterator protocol integration for `for-of` / `for await-of`.
//!
//! Spec: ECMA-262 §14 (Statements and Declarations).

use super::module_compiler::ModuleCompiler;
use super::shared::{Binding, FinallyScope, FunctionCompiler, LoopScope, ValueLocation};
use super::*;
use crate::bytecode::ProgramCounter;

impl<'a> FunctionCompiler<'a> {
    pub(super) fn compile_switch_statement(
        &mut self,
        switch: &oxc_ast::ast::SwitchStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let discriminant = self.compile_expression(&switch.discriminant, module)?;
        let discriminant = self.materialize_value(discriminant);

        // Each case: compare discriminant === test, jump to body if match.
        // Fall-through between cases is the default JS behavior.
        let mut case_body_starts: Vec<usize> = Vec::new();
        let mut case_jumps: Vec<usize> = Vec::new();
        let mut default_index: Option<usize> = None;

        // Phase 1: emit comparison + conditional jumps for each case.
        for (i, case) in switch.cases.iter().enumerate() {
            if case.test.is_none() {
                default_index = Some(i);
                case_jumps.push(0); // placeholder, patched later
                continue;
            }
            let test = self.compile_expression(case.test.as_ref().unwrap(), module)?;
            let cmp_result = ValueLocation::temp(self.alloc_temp());
            self.instructions.push(Instruction::eq(
                cmp_result.register,
                discriminant.register,
                test.register,
            ));
            self.release(test);
            let jump = self.emit_conditional_placeholder(Opcode::JumpIfTrue, cmp_result.register);
            self.release(cmp_result);
            case_jumps.push(jump);
        }

        // Jump to default or end if no case matched.
        let jump_to_default_or_end = self.emit_jump_placeholder();
        self.release(discriminant);

        // Push a loop scope so `break` works inside switch.
        self.loop_stack.push(LoopScope {
            continue_target: None,
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            iterator_register: None,
            label: None,
        });

        // Phase 2: emit case bodies with fall-through.
        for (i, case) in switch.cases.iter().enumerate() {
            let body_start = self.instructions.len();
            case_body_starts.push(body_start);
            // Patch the case's conditional jump to this body.
            if case.test.is_some() {
                self.patch_jump(case_jumps[i], body_start)?;
            }
            let mut body_terminated = false;
            for stmt in &case.consequent {
                if body_terminated {
                    break;
                }
                body_terminated = self.compile_statement(stmt, module)?;
            }
        }

        // Patch default jump.
        if let Some(idx) = default_index {
            self.patch_jump(jump_to_default_or_end, case_body_starts[idx])?;
        } else {
            self.patch_jump(jump_to_default_or_end, self.instructions.len())?;
        }

        // Pop loop scope and patch break jumps.
        let loop_scope = self.loop_stack.pop().expect("switch scope must exist");
        let end = self.instructions.len();
        for jump in loop_scope.break_jumps {
            self.patch_jump(jump, end)?;
        }

        // A switch statement never terminates the enclosing block:
        // - `break` exits the switch and continues after it
        // - Even if all cases return/throw, the "no match + no default" path is reachable
        Ok(false)
    }

    pub(super) fn compile_for_statement(
        &mut self,
        for_statement: &oxc_ast::ast::ForStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        if let Some(init) = &for_statement.init {
            match init {
                oxc_ast::ast::ForStatementInit::VariableDeclaration(declaration) => {
                    self.compile_variable_declaration(declaration, module)?;
                }
                _ => {
                    let expression = init.to_expression();
                    let value = self.compile_expression(expression, module)?;
                    self.release(value);
                }
            }
        }

        let loop_start = self.instructions.len();
        let exit_jump = if let Some(test) = &for_statement.test {
            let condition = self.compile_expression(test, module)?;
            let jump = self.emit_conditional_placeholder(Opcode::JumpIfFalse, condition.register);
            self.release(condition);
            Some(jump)
        } else {
            None
        };

        self.loop_stack.push(LoopScope {
            continue_target: None,
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            iterator_register: None,
            label: self.pending_loop_label.take(),
        });

        let _ = self.compile_statement(&for_statement.body, module)?;

        let continue_target = self.instructions.len();
        if let Some(loop_scope) = self.loop_stack.last_mut() {
            loop_scope.continue_target = Some(continue_target);
        }

        if let Some(update) = &for_statement.update {
            let value = self.compile_expression(update, module)?;
            self.release(value);
        }

        self.emit_relative_jump(loop_start)?;
        let loop_end = self.instructions.len();
        if let Some(exit_jump) = exit_jump {
            self.patch_jump(exit_jump, loop_end)?;
        }
        let loop_scope = self.loop_stack.pop().expect("loop scope must exist");
        self.patch_loop_scope(loop_scope, loop_end, continue_target)?;

        Ok(false)
    }

    pub(super) fn compile_for_of_statement(
        &mut self,
        for_of_statement: &oxc_ast::ast::ForOfStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        if for_of_statement.r#await {
            return self.compile_for_await_of_statement(for_of_statement, module);
        }

        let iterator_register = self.allocate_local()?;
        let done_register = self.allocate_local()?;
        let value_register = self.allocate_local()?;
        let exception_register = self.allocate_local()?;

        let iterable = self.compile_expression(&for_of_statement.right, module)?;
        self.instructions.push(Instruction::get_iterator(
            iterator_register,
            iterable.register,
        ));
        self.release(iterable);

        let try_start = self.instructions.len();
        let loop_start = self.instructions.len();
        self.loop_stack.push(LoopScope {
            continue_target: Some(loop_start),
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            iterator_register: Some(iterator_register),
            label: self.pending_loop_label.take(),
        });

        self.instructions.push(Instruction::iterator_next(
            done_register,
            value_register,
            iterator_register,
        ));
        let jump_to_exit = self.emit_conditional_placeholder(Opcode::JumpIfTrue, done_register);

        self.assign_for_of_left(&for_of_statement.left, value_register, module)?;
        let _ = self.compile_statement(&for_of_statement.body, module)?;
        self.emit_relative_jump(loop_start)?;

        let loop_scope = self.loop_stack.pop().expect("loop scope must exist");
        let normal_exit_pc = self.instructions.len();
        self.patch_jump(jump_to_exit, normal_exit_pc)?;
        self.patch_loop_scope(loop_scope, normal_exit_pc, loop_start)?;

        let jump_over_exception_handler = self.emit_jump_placeholder();
        let exception_handler_pc = self.instructions.len();
        self.instructions
            .push(Instruction::load_exception(exception_register));
        self.instructions
            .push(Instruction::iterator_close(iterator_register));
        self.instructions
            .push(Instruction::throw(exception_register));
        self.patch_jump(jump_over_exception_handler, self.instructions.len())?;

        self.exception_handlers.push(ExceptionHandler::new(
            try_start as ProgramCounter,
            normal_exit_pc as ProgramCounter,
            exception_handler_pc as ProgramCounter,
        ));

        Ok(false)
    }

    /// ES2024 §14.7.5.6 ForIn/OfHeadEvaluation — `for await (... of ...)` variant.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-forinofheadevaluation>
    ///
    /// Compiles to: GetAsyncIterator → loop { call .next(), Await, extract
    /// done/value } with exception-handler close.
    fn compile_for_await_of_statement(
        &mut self,
        for_of: &oxc_ast::ast::ForOfStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let iterator_register = self.allocate_local()?;
        let next_fn_register = self.allocate_local()?;
        let promise_register = self.allocate_local()?;
        let result_register = self.allocate_local()?;
        let done_register = self.allocate_local()?;
        let value_register = self.allocate_local()?;
        let exception_register = self.allocate_local()?;

        // 1. Evaluate the iterable expression.
        let iterable = self.compile_expression(&for_of.right, module)?;
        // 2. GetIterator(iterable, async).
        self.instructions.push(Instruction::get_async_iterator(
            iterator_register,
            iterable.register,
        ));
        self.release(iterable);

        // 3. Cache the .next() method on the iterator.
        let next_prop = self.intern_property_name("next")?;
        self.instructions.push(Instruction::get_property(
            next_fn_register,
            iterator_register,
            next_prop,
        ));

        let try_start = self.instructions.len();
        let loop_start = self.instructions.len();
        self.loop_stack.push(LoopScope {
            continue_target: Some(loop_start),
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            iterator_register: Some(iterator_register),
            label: self.pending_loop_label.take(),
        });

        // 4. Call iterator.next() — 0 args, receiver = iterator.
        let arg_start = BytecodeRegister::new(self.next_local + self.next_temp);
        let call_pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            promise_register,
            next_fn_register,
            arg_start,
        ));
        self.record_call_site(
            call_pc,
            CallSite::Closure(ClosureCall::new_with_receiver(
                0,
                FrameFlags::new(false, true, false),
                iterator_register,
            )),
        );

        // 5. Await the result promise → {done, value}.
        self.instructions
            .push(Instruction::r#await(result_register, promise_register));

        // 6. Extract done: let done = result.done.
        let done_prop = self.intern_property_name("done")?;
        self.instructions.push(Instruction::get_property(
            done_register,
            result_register,
            done_prop,
        ));
        let jump_to_exit = self.emit_conditional_placeholder(Opcode::JumpIfTrue, done_register);

        // 7. Extract value: let value = result.value.
        let value_prop = self.intern_property_name("value")?;
        self.instructions.push(Instruction::get_property(
            value_register,
            result_register,
            value_prop,
        ));

        // 8. Assign to the loop variable and compile body.
        self.assign_for_of_left(&for_of.left, value_register, module)?;
        let _ = self.compile_statement(&for_of.body, module)?;
        self.emit_relative_jump(loop_start)?;

        let loop_scope = self.loop_stack.pop().expect("loop scope must exist");
        let normal_exit_pc = self.instructions.len();
        self.patch_jump(jump_to_exit, normal_exit_pc)?;
        self.patch_loop_scope(loop_scope, normal_exit_pc, loop_start)?;

        // 9. Exception handler: close iterator and rethrow.
        let jump_over_exception_handler = self.emit_jump_placeholder();
        let exception_handler_pc = self.instructions.len();
        self.instructions
            .push(Instruction::load_exception(exception_register));
        self.instructions
            .push(Instruction::iterator_close(iterator_register));
        self.instructions
            .push(Instruction::throw(exception_register));
        self.patch_jump(jump_over_exception_handler, self.instructions.len())?;

        self.exception_handlers.push(ExceptionHandler::new(
            try_start as ProgramCounter,
            normal_exit_pc as ProgramCounter,
            exception_handler_pc as ProgramCounter,
        ));

        Ok(false)
    }

    pub(super) fn compile_for_in_statement(
        &mut self,
        for_in_statement: &oxc_ast::ast::ForInStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let iterator_register = self.allocate_local()?;
        let done_register = self.allocate_local()?;
        let value_register = self.allocate_local()?;

        let object = self.compile_expression(&for_in_statement.right, module)?;
        self.instructions.push(Instruction::get_property_iterator(
            iterator_register,
            object.register,
        ));
        self.release(object);

        let loop_start = self.instructions.len();
        self.loop_stack.push(LoopScope {
            continue_target: Some(loop_start),
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            iterator_register: None,
            label: self.pending_loop_label.take(),
        });

        self.instructions.push(Instruction::property_iterator_next(
            done_register,
            value_register,
            iterator_register,
        ));
        let jump_to_exit = self.emit_conditional_placeholder(Opcode::JumpIfTrue, done_register);

        self.assign_for_of_left(&for_in_statement.left, value_register, module)?;
        let _ = self.compile_statement(&for_in_statement.body, module)?;
        self.emit_relative_jump(loop_start)?;

        let loop_scope = self.loop_stack.pop().expect("loop scope must exist");
        let loop_end = self.instructions.len();
        self.patch_jump(jump_to_exit, loop_end)?;
        self.patch_loop_scope(loop_scope, loop_end, loop_start)?;

        Ok(false)
    }

    /// §13.7.5.12 ForIn/OfBodyEvaluation — assign the iteration value to
    /// the for-of / for-in / for-await-of left-hand side binding.
    ///
    /// Handles:
    /// - Simple identifiers: `for (const x of arr)`
    /// - Destructuring bindings: `for (const {a, b} of arr)`, `for (const [a, b] of arr)`
    /// - Assignment targets: `for ({a, b} of arr)`, `for ([a, b] of arr)`
    /// - Member expressions: `for (obj.prop of arr)`, `for (obj[key] of arr)`
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-forinofbodyevaluation>
    pub(super) fn assign_for_of_left(
        &mut self,
        left: &ForStatementLeft<'_>,
        value_register: BytecodeRegister,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        match left {
            ForStatementLeft::VariableDeclaration(declaration) => {
                if declaration.declarations.len() != 1 {
                    return Err(SourceLoweringError::Unsupported(
                        "multiple for..of declarators".to_string(),
                    ));
                }

                let declarator = declaration
                    .declarations
                    .first()
                    .expect("single declarator must exist");
                if declarator.init.is_some() {
                    return Err(SourceLoweringError::Unsupported(
                        "for..of declaration initializers".to_string(),
                    ));
                }

                // §13.15.5 Destructuring binding patterns in for-of/for-in.
                // Spec: <https://tc39.es/ecma262/#sec-destructuring-assignment>
                match &declarator.id {
                    BindingPattern::BindingIdentifier(identifier) => {
                        let register =
                            self.declare_variable_binding(identifier.name.as_str(), true)?;
                        self.assign_binding(
                            identifier.name.as_str(),
                            register,
                            ValueLocation::local(value_register),
                        )?;
                    }
                    pattern @ (BindingPattern::ObjectPattern(_)
                    | BindingPattern::ArrayPattern(_)
                    | BindingPattern::AssignmentPattern(_)) => {
                        self.compile_binding_pattern_target(
                            pattern,
                            ValueLocation::local(value_register),
                            false,
                            module,
                        )?;
                    }
                }
                Ok(())
            }
            ForStatementLeft::AssignmentTargetIdentifier(identifier) => {
                let _ = self.assign_to_name(
                    identifier.name.as_str(),
                    ValueLocation::local(value_register),
                )?;
                Ok(())
            }
            ForStatementLeft::ComputedMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                self.store_computed_member(
                    object,
                    module,
                    member,
                    ValueLocation::local(value_register),
                )
            }
            ForStatementLeft::StaticMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                let property = self.intern_property_name(member.property.name.as_str())?;
                self.instructions.push(Instruction::set_property(
                    object.register,
                    value_register,
                    property,
                ));
                self.release(object);
                Ok(())
            }
            // §13.15.5 Destructuring assignment targets in for-of/for-in.
            // `for ({a, b} of arr)` or `for ([a, b] of arr)`
            ForStatementLeft::ObjectAssignmentTarget(target) => {
                let value = self.stabilize_binding_value(ValueLocation::local(value_register))?;
                self.compile_object_assignment_destructuring(target, value.register, module)?;
                self.release(value);
                Ok(())
            }
            ForStatementLeft::ArrayAssignmentTarget(target) => {
                let value = self.stabilize_binding_value(ValueLocation::local(value_register))?;
                self.compile_array_assignment_destructuring(target, value.register, module)?;
                self.release(value);
                Ok(())
            }
            _ => Err(SourceLoweringError::Unsupported(
                "for..of left-hand side".to_string(),
            )),
        }
    }

    pub(super) fn find_loop_scope_index(&self, label: Option<&str>) -> Option<usize> {
        match label {
            None => {
                // Unlabeled: find the innermost scope.
                self.loop_stack.len().checked_sub(1)
            }
            Some(label) => self
                .loop_stack
                .iter()
                .enumerate()
                .rev()
                .find(|(_, scope)| scope.label.as_deref() == Some(label))
                .map(|(i, _)| i),
        }
    }

    pub(super) fn compile_break_statement(
        &mut self,
        break_statement: &oxc_ast::ast::BreakStatement<'_>,
    ) -> Result<bool, SourceLoweringError> {
        let label = break_statement.label.as_ref().map(|l| l.name.as_str());
        let index = self.find_loop_scope_index(label).ok_or_else(|| {
            SourceLoweringError::Unsupported("break outside of a loop".to_string())
        })?;

        let iterator_register = self.loop_stack[index].iterator_register;
        if let Some(iterator_register) = iterator_register {
            self.instructions
                .push(Instruction::iterator_close(iterator_register));
        }

        let jump = self.emit_jump_placeholder();
        self.loop_stack[index].break_jumps.push(jump);
        Ok(true)
    }

    pub(super) fn compile_continue_statement(
        &mut self,
        continue_statement: &oxc_ast::ast::ContinueStatement<'_>,
    ) -> Result<bool, SourceLoweringError> {
        let label = continue_statement.label.as_ref().map(|l| l.name.as_str());
        let index = if let Some(lbl) = label {
            self.find_loop_scope_index(Some(lbl)).ok_or_else(|| {
                SourceLoweringError::Unsupported(format!("continue with unknown label '{lbl}'"))
            })?
        } else {
            // Unlabeled continue: use the innermost loop scope. Note that `continue_target`
            // may not be set yet for `for` loops (it's set after the body is compiled), so
            // we cannot filter by that field here.
            self.loop_stack.len().checked_sub(1).ok_or_else(|| {
                SourceLoweringError::Unsupported("continue outside of a loop".to_string())
            })?
        };

        let jump = self.emit_jump_placeholder();
        self.loop_stack[index].continue_jumps.push(jump);
        Ok(true)
    }

    pub(super) fn compile_labeled_statement(
        &mut self,
        labeled: &oxc_ast::ast::LabeledStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let label = labeled.label.name.to_string();
        self.pending_loop_label = Some(label.clone());
        let result = self.compile_statement(&labeled.body, module)?;
        // If the label wasn't consumed by a loop (e.g., labeled block), clear it.
        self.pending_loop_label = None;
        Ok(result)
    }

    pub(super) fn compile_try_statement(
        &mut self,
        try_statement: &oxc_ast::ast::TryStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        if let Some(finalizer) = &try_statement.finalizer {
            return self.compile_try_with_finally(try_statement, finalizer, module);
        }

        let handler = try_statement.handler.as_ref().ok_or_else(|| {
            SourceLoweringError::Unsupported("try without catch or finally".to_string())
        })?;
        self.compile_try_catch_without_finally(&try_statement.block.body, handler, module)
    }

    fn compile_try_catch_without_finally(
        &mut self,
        try_body: &[AstStatement<'_>],
        handler: &oxc_ast::ast::CatchClause<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let try_start = self.instructions.len();
        let try_terminated = self.compile_statements(try_body, module)?;
        let try_end = self.instructions.len();
        let jump_over_catch = if try_terminated {
            None
        } else {
            Some(self.emit_jump_placeholder())
        };

        let catch_pc = self.instructions.len();
        let catch_terminated = self.compile_catch_clause(handler, module)?;
        self.exception_handlers.push(ExceptionHandler::new(
            try_start as ProgramCounter,
            try_end as ProgramCounter,
            catch_pc as ProgramCounter,
        ));

        if let Some(jump_over_catch) = jump_over_catch {
            self.patch_jump(jump_over_catch, self.instructions.len())?;
        }

        Ok(try_terminated && catch_terminated)
    }

    fn compile_try_with_finally(
        &mut self,
        try_statement: &oxc_ast::ast::TryStatement<'_>,
        finalizer: &oxc_ast::ast::BlockStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let return_flag_register = self.allocate_local()?;
        let return_value_register = self.allocate_local()?;
        let initialized = self.load_bool_into_register(return_flag_register, false)?;
        self.release(initialized);
        let value_init = self.load_undefined()?;
        if value_init.register != return_value_register {
            self.instructions.push(Instruction::move_(
                return_value_register,
                value_init.register,
            ));
        }
        self.release(value_init);

        self.finally_stack.push(FinallyScope {
            return_flag_register,
            return_value_register,
            return_jumps: Vec::new(),
        });

        let try_start = self.instructions.len();
        let try_terminated = self.compile_statements(&try_statement.block.body, module)?;
        let try_end = self.instructions.len();

        let handler = try_statement.handler.as_ref();
        let jump_over_catch = if handler.is_some() && !try_terminated {
            Some(self.emit_jump_placeholder())
        } else {
            None
        };

        let mut catch_range = None;
        let _catch_terminated = if let Some(handler) = handler {
            let catch_pc = self.instructions.len();
            let catch_terminated = self.compile_catch_clause(handler, module)?;
            let catch_end = self.instructions.len();
            catch_range = Some((catch_pc, catch_end));
            self.exception_handlers.push(ExceptionHandler::new(
                try_start as ProgramCounter,
                try_end as ProgramCounter,
                catch_pc as ProgramCounter,
            ));
            catch_terminated
        } else {
            false
        };

        if let Some(jump_over_catch) = jump_over_catch {
            self.patch_jump(jump_over_catch, self.instructions.len())?;
        }

        let finally_scope = self.finally_stack.pop().expect("finally scope must exist");
        let normal_finally_pc = self.instructions.len();
        for jump in finally_scope.return_jumps {
            self.patch_jump(jump, normal_finally_pc)?;
        }

        let normal_finally_terminated = self.compile_finalizer_body(&finalizer.body, module)?;
        if !normal_finally_terminated {
            let deferred_return_end = self.emit_conditional_placeholder(
                Opcode::JumpIfFalse,
                finally_scope.return_flag_register,
            );
            self.instructions
                .push(Instruction::ret(finally_scope.return_value_register));
            self.patch_jump(deferred_return_end, self.instructions.len())?;
        }

        let jump_over_exception_finally = if normal_finally_terminated {
            None
        } else {
            Some(self.emit_jump_placeholder())
        };

        let exception_finally_pc = self.instructions.len();
        let exception_register = self.allocate_local()?;
        self.instructions
            .push(Instruction::load_exception(exception_register));
        let exception_finally_terminated = self.compile_finalizer_body(&finalizer.body, module)?;
        if !exception_finally_terminated {
            self.instructions
                .push(Instruction::throw(exception_register));
        }

        if handler.is_none() {
            self.exception_handlers.push(ExceptionHandler::new(
                try_start as ProgramCounter,
                try_end as ProgramCounter,
                exception_finally_pc as ProgramCounter,
            ));
        }
        if let Some((catch_start, catch_end)) = catch_range
            && catch_start < catch_end
        {
            self.exception_handlers.push(ExceptionHandler::new(
                catch_start as ProgramCounter,
                catch_end as ProgramCounter,
                exception_finally_pc as ProgramCounter,
            ));
        }

        if let Some(jump_over_exception_finally) = jump_over_exception_finally {
            self.patch_jump(jump_over_exception_finally, self.instructions.len())?;
        }

        Ok(normal_finally_terminated)
    }

    fn compile_catch_clause(
        &mut self,
        handler: &oxc_ast::ast::CatchClause<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        // Save the bindings map so the catch parameter and any inner
        // declarations are scoped to the catch body. We restore *only*
        // bindings; captures and capture_ids accumulate across the whole
        // function and must persist past the catch body.
        let saved_bindings = self.scope.borrow().bindings.clone();

        // §14.15.2 CatchClauseEvaluation — bind the caught exception to
        // the catch parameter (simple identifier or destructuring pattern).
        // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-catchclauseevaluation>
        if let Some(param) = &handler.param {
            match &param.pattern {
                BindingPattern::BindingIdentifier(identifier) => {
                    let register = self.allocate_local()?;
                    self.scope
                        .borrow_mut()
                        .bindings
                        .insert(identifier.name.to_string(), Binding::Register(register));
                    self.instructions
                        .push(Instruction::load_exception(register));
                }
                pattern @ (BindingPattern::ObjectPattern(_)
                | BindingPattern::ArrayPattern(_)
                | BindingPattern::AssignmentPattern(_)) => {
                    let exc_reg = self.allocate_local()?;
                    self.instructions.push(Instruction::load_exception(exc_reg));
                    self.compile_binding_pattern_target(
                        pattern,
                        ValueLocation::local(exc_reg),
                        false,
                        module,
                    )?;
                }
            }
        }

        let terminated = self.compile_statements(&handler.body.body, module)?;
        self.scope.borrow_mut().bindings = saved_bindings;
        Ok(terminated)
    }

    fn compile_finalizer_body(
        &mut self,
        statements: &[AstStatement<'_>],
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        self.compile_statements(statements, module)
    }
}
