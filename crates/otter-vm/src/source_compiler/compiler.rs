use super::ast::{
    collect_function_declarations, collect_var_names, extract_function_params,
    is_test262_failure_throw,
};
use super::module_compiler::{FunctionIdentity, ModuleCompiler};
use super::shared::{
    Binding, CaptureSource, CompileEnv, CompiledFunction, FinallyScope, FunctionCompiler,
    FunctionKind, LoopScope, PendingFunction, ValueLocation,
};
use super::*;
use crate::bytecode::ProgramCounter;

impl<'a> FunctionCompiler<'a> {
    pub(super) fn new(
        mode: LoweringMode,
        function_name: Option<String>,
        kind: FunctionKind,
        parent_env: Option<CompileEnv>,
    ) -> Self {
        Self {
            mode,
            function_name,
            kind,
            parent_env,
            env: CompileEnv::new(),
            next_local: 0,
            parameter_count: 0,
            next_temp: 0,
            max_temp: 0,
            instructions: Vec::new(),
            property_names: Vec::new(),
            property_name_ids: BTreeMap::new(),
            string_literals: Vec::new(),
            string_ids: BTreeMap::new(),
            closure_templates: Vec::new(),
            call_sites: Vec::new(),
            exception_handlers: Vec::new(),
            captures: Vec::new(),
            capture_ids: BTreeMap::new(),
            hoisted_functions: Vec::new(),
            finally_stack: Vec::new(),
            loop_stack: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }

    pub(super) fn declare_parameters(
        &mut self,
        params: &[&str],
    ) -> Result<(), SourceLoweringError> {
        for name in params {
            let register = BytecodeRegister::new(self.parameter_count);
            self.parameter_count = self
                .parameter_count
                .checked_add(1)
                .ok_or(SourceLoweringError::TooManyLocals)?;
            self.next_local = self.parameter_count;
            self.env
                .bindings
                .insert((*name).to_string(), Binding::Register(register));
        }
        Ok(())
    }

    pub(super) fn declare_test262_intrinsic_globals(&mut self) -> Result<(), SourceLoweringError> {
        for name in ["Object", "Function", "Math", "Array", "Reflect", "String"] {
            self.declare_intrinsic_global_binding(name)?;
        }
        Ok(())
    }

    pub(super) fn predeclare_function_scope(
        &mut self,
        statements: &[AstStatement<'_>],
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        for name in collect_var_names(statements) {
            self.declare_variable_binding(&name, true)?;
        }

        let mut reserved = Vec::new();
        collect_function_declarations(statements, &mut reserved);
        for function in reserved {
            let name = function.id.as_ref().ok_or_else(|| {
                SourceLoweringError::Unsupported(
                    "function declarations without identifiers".to_string(),
                )
            })?;

            let reserved_index = module.reserve_function();
            let closure_register = self.declare_function_binding(name.name.as_str())?;
            let pending = PendingFunction {
                reserved: reserved_index,
                closure_register,
                captures: Vec::new(),
            };

            let params = extract_function_params(function)?;
            let compiled = module.compile_function_from_statements(
                reserved_index,
                FunctionIdentity {
                    debug_name: Some(name.name.to_string()),
                    self_binding_name: Some(name.name.to_string()),
                },
                function
                    .body
                    .as_ref()
                    .map(|body| body.statements.as_slice())
                    .ok_or_else(|| {
                        SourceLoweringError::Unsupported(
                            "function declarations without bodies".to_string(),
                        )
                    })?,
                &params,
                FunctionKind::Ordinary,
                Some(self.env.clone()),
            )?;
            module.set_function(reserved_index, compiled.function);
            self.hoisted_functions.push(PendingFunction {
                captures: compiled.captures,
                ..pending
            });
        }

        Ok(())
    }

    pub(super) fn emit_hoisted_function_initializers(&mut self) -> Result<(), SourceLoweringError> {
        for pending in self.hoisted_functions.clone() {
            self.emit_new_closure(
                pending.closure_register,
                pending.reserved,
                &pending.captures,
            )?;
        }
        Ok(())
    }

    pub(super) fn finish(
        self,
        _function_index: FunctionIndex,
        name: Option<&str>,
    ) -> Result<CompiledFunction, SourceLoweringError> {
        let frame_layout = FrameLayout::new(
            1,
            self.parameter_count,
            self.next_local.saturating_sub(self.parameter_count),
            self.max_temp,
        )
        .map_err(|_| SourceLoweringError::TooManyLocals)?;

        let tables = FunctionTables::new(
            FunctionSideTables::new(
                PropertyNameTable::new(self.property_names),
                StringTable::new(self.string_literals),
                ClosureTable::new(self.closure_templates),
                CallTable::new(self.call_sites),
            ),
            FeedbackTableLayout::default(),
            DeoptTable::default(),
            ExceptionTable::new(self.exception_handlers),
            SourceMap::default(),
        );

        Ok(CompiledFunction {
            function: VmFunction::new(
                name,
                frame_layout,
                Bytecode::from(self.instructions),
                tables,
            ),
            captures: self.captures,
        })
    }

    pub(super) fn compile_statements(
        &mut self,
        statements: &[AstStatement<'_>],
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let mut terminated = false;
        for statement in statements {
            if terminated {
                break;
            }
            terminated = self.compile_statement(statement, module)?;
        }
        Ok(terminated)
    }

    pub(super) fn compile_statement(
        &mut self,
        statement: &AstStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        match statement {
            AstStatement::EmptyStatement(_) => Ok(false),
            AstStatement::BlockStatement(block) => self.compile_statements(&block.body, module),
            AstStatement::FunctionDeclaration(_) => Ok(false),
            AstStatement::VariableDeclaration(declaration) => {
                self.compile_variable_declaration(declaration, module)?;
                Ok(false)
            }
            AstStatement::ExpressionStatement(expression_statement) => {
                self.compile_expression_statement(&expression_statement.expression, module)?;
                Ok(false)
            }
            AstStatement::IfStatement(if_statement) => {
                let condition = self.compile_expression(&if_statement.test, module)?;
                let jump_to_else =
                    self.emit_conditional_placeholder(Opcode::JumpIfFalse, condition.register);
                self.release(condition);

                let then_terminated = self.compile_statement(&if_statement.consequent, module)?;
                let jump_to_end = if if_statement.alternate.is_some() {
                    Some(self.emit_jump_placeholder())
                } else {
                    None
                };

                self.patch_jump(jump_to_else, self.instructions.len())?;
                let else_terminated = if let Some(alternate) = &if_statement.alternate {
                    self.compile_statement(alternate, module)?
                } else {
                    false
                };

                if let Some(jump_to_end) = jump_to_end {
                    self.patch_jump(jump_to_end, self.instructions.len())?;
                }

                Ok(then_terminated && else_terminated)
            }
            AstStatement::WhileStatement(while_statement) => {
                let loop_start = self.instructions.len();
                let condition = self.compile_expression(&while_statement.test, module)?;
                let exit_jump =
                    self.emit_conditional_placeholder(Opcode::JumpIfFalse, condition.register);
                self.release(condition);
                self.loop_stack.push(LoopScope {
                    continue_target: Some(loop_start),
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    iterator_register: None,
                });
                let _ = self.compile_statement(&while_statement.body, module)?;
                let loop_scope = self.loop_stack.pop().expect("loop scope must exist");
                self.emit_relative_jump(loop_start)?;
                let loop_end = self.instructions.len();
                self.patch_jump(exit_jump, loop_end)?;
                self.patch_loop_scope(loop_scope, loop_end, loop_start)?;
                Ok(false)
            }
            AstStatement::DoWhileStatement(do_while_statement) => {
                let loop_start = self.instructions.len();
                self.loop_stack.push(LoopScope {
                    continue_target: None,
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    iterator_register: None,
                });
                if self.compile_statement(&do_while_statement.body, module)? {
                    let loop_scope = self.loop_stack.pop().expect("loop scope must exist");
                    let continue_target = self.instructions.len();
                    self.patch_loop_scope(loop_scope, continue_target, continue_target)?;
                    return Ok(true);
                }

                let continue_target = self.instructions.len();
                if let Some(loop_scope) = self.loop_stack.last_mut() {
                    loop_scope.continue_target = Some(continue_target);
                }
                let condition = self.compile_expression(&do_while_statement.test, module)?;
                let back_jump =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, condition.register);
                self.release(condition);
                self.patch_jump(back_jump, loop_start)?;
                let loop_scope = self.loop_stack.pop().expect("loop scope must exist");
                self.patch_loop_scope(loop_scope, self.instructions.len(), continue_target)?;
                Ok(false)
            }
            AstStatement::ForOfStatement(for_of_statement) => {
                self.compile_for_of_statement(for_of_statement, module)
            }
            AstStatement::TryStatement(try_statement) => {
                self.compile_try_statement(try_statement, module)
            }
            AstStatement::BreakStatement(break_statement) => {
                self.compile_break_statement(break_statement)
            }
            AstStatement::ContinueStatement(continue_statement) => {
                self.compile_continue_statement(continue_statement)
            }
            AstStatement::ReturnStatement(return_statement) => {
                self.compile_return_statement(return_statement, module)
            }
            AstStatement::ThrowStatement(throw_statement)
                if self.mode == LoweringMode::Test262Basic
                    && is_test262_failure_throw(&throw_statement.argument) =>
            {
                let result = self.load_i32(1)?;
                self.instructions.push(Instruction::ret(result.register));
                self.release(result);
                Ok(true)
            }
            AstStatement::ThrowStatement(throw_statement) => {
                let value = self.compile_expression(&throw_statement.argument, module)?;
                self.emit_iterator_closes_for_active_loops();
                self.instructions.push(Instruction::throw(value.register));
                self.release(value);
                Ok(true)
            }
            _ => Err(SourceLoweringError::Unsupported(format!(
                "statement {:?}",
                statement
            ))),
        }
    }

    pub(super) fn compile_variable_declaration(
        &mut self,
        declaration: &oxc_ast::ast::VariableDeclaration<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        match declaration.kind {
            VariableDeclarationKind::Var
            | VariableDeclarationKind::Let
            | VariableDeclarationKind::Const => {}
            _ => {
                return Err(SourceLoweringError::Unsupported(format!(
                    "variable declaration kind {:?}",
                    declaration.kind
                )));
            }
        }

        for declarator in &declaration.declarations {
            let BindingPattern::BindingIdentifier(identifier) = &declarator.id else {
                return Err(SourceLoweringError::Unsupported(
                    "destructuring bindings".to_string(),
                ));
            };

            let register = self.declare_variable_binding(
                identifier.name.as_str(),
                declaration.kind == VariableDeclarationKind::Var,
            )?;
            if let Some(init) = &declarator.init {
                let value = self.compile_expression(init, module)?;
                self.assign_binding(identifier.name.as_str(), register, value)?;
            }
        }

        Ok(())
    }

    pub(super) fn compile_expression_statement(
        &mut self,
        expression: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        if matches!(expression, Expression::StringLiteral(_)) {
            return Ok(());
        }

        let value = self.compile_expression(expression, module)?;
        self.release(value);
        Ok(())
    }

    fn compile_return_statement(
        &mut self,
        return_statement: &oxc_ast::ast::ReturnStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let value = if let Some(argument) = &return_statement.argument {
            self.compile_expression(argument, module)?
        } else {
            self.load_undefined()?
        };

        if let Some(scope_index) = self.finally_stack.len().checked_sub(1) {
            let return_flag_register = self.finally_stack[scope_index].return_flag_register;
            let return_value_register = self.finally_stack[scope_index].return_value_register;
            let flag = self.load_bool_into_register(return_flag_register, true)?;
            self.release(flag);

            if value.register != return_value_register {
                self.instructions
                    .push(Instruction::move_(return_value_register, value.register));
            }
            self.release(value);

            let jump = self.emit_jump_placeholder();
            self.finally_stack[scope_index].return_jumps.push(jump);
            return Ok(true);
        }

        self.emit_iterator_closes_for_active_loops();
        self.instructions.push(Instruction::ret(value.register));
        self.release(value);
        Ok(true)
    }

    fn compile_for_of_statement(
        &mut self,
        for_of_statement: &oxc_ast::ast::ForOfStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        if for_of_statement.r#await {
            return Err(SourceLoweringError::Unsupported(
                "for await..of".to_string(),
            ));
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

    fn assign_for_of_left(
        &mut self,
        left: &ForStatementLeft<'_>,
        value_register: BytecodeRegister,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        match left {
            ForStatementLeft::VariableDeclaration(declaration) => {
                if declaration.kind != VariableDeclarationKind::Var {
                    return Err(SourceLoweringError::Unsupported(
                        "for..of lexical declarations".to_string(),
                    ));
                }
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
                let BindingPattern::BindingIdentifier(identifier) = &declarator.id else {
                    return Err(SourceLoweringError::Unsupported(
                        "for..of destructuring bindings".to_string(),
                    ));
                };

                let register = self.declare_variable_binding(identifier.name.as_str(), true)?;
                self.assign_binding(
                    identifier.name.as_str(),
                    register,
                    ValueLocation::local(value_register),
                )?;
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
            _ => Err(SourceLoweringError::Unsupported(
                "for..of left-hand side".to_string(),
            )),
        }
    }

    fn compile_break_statement(
        &mut self,
        break_statement: &oxc_ast::ast::BreakStatement<'_>,
    ) -> Result<bool, SourceLoweringError> {
        if break_statement.label.is_some() {
            return Err(SourceLoweringError::Unsupported(
                "labeled break".to_string(),
            ));
        }

        let iterator_register = self
            .loop_stack
            .last()
            .ok_or_else(|| SourceLoweringError::Unsupported("break outside of a loop".to_string()))?
            .iterator_register;
        if let Some(iterator_register) = iterator_register {
            self.instructions
                .push(Instruction::iterator_close(iterator_register));
        }

        let jump = self.emit_jump_placeholder();
        self.loop_stack
            .last_mut()
            .expect("loop scope must exist")
            .break_jumps
            .push(jump);
        Ok(true)
    }

    fn compile_continue_statement(
        &mut self,
        continue_statement: &oxc_ast::ast::ContinueStatement<'_>,
    ) -> Result<bool, SourceLoweringError> {
        if continue_statement.label.is_some() {
            return Err(SourceLoweringError::Unsupported(
                "labeled continue".to_string(),
            ));
        }

        let jump = self.emit_jump_placeholder();
        self.loop_stack
            .last_mut()
            .ok_or_else(|| {
                SourceLoweringError::Unsupported("continue outside of a loop".to_string())
            })?
            .continue_jumps
            .push(jump);
        Ok(true)
    }

    fn compile_try_statement(
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
        let saved_env = self.env.clone();

        if let Some(param) = &handler.param {
            let BindingPattern::BindingIdentifier(identifier) = &param.pattern else {
                return Err(SourceLoweringError::Unsupported(
                    "destructuring catch parameters".to_string(),
                ));
            };
            let register = self.allocate_local()?;
            self.env
                .bindings
                .insert(identifier.name.to_string(), Binding::Register(register));
            self.instructions
                .push(Instruction::load_exception(register));
        }

        let terminated = self.compile_statements(&handler.body.body, module)?;
        self.env = saved_env;
        Ok(terminated)
    }

    fn compile_finalizer_body(
        &mut self,
        statements: &[AstStatement<'_>],
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        self.compile_statements(statements, module)
    }

    fn load_bool_into_register(
        &mut self,
        destination: BytecodeRegister,
        value: bool,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let loaded = self.compile_bool(value)?;
        if loaded.register != destination {
            self.instructions
                .push(Instruction::move_(destination, loaded.register));
        }
        Ok(loaded)
    }

    pub(super) fn compile_bool(
        &mut self,
        value: bool,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        self.instructions.push(if value {
            Instruction::load_true(register)
        } else {
            Instruction::load_false(register)
        });
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn load_i32(&mut self, value: i32) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        self.instructions
            .push(Instruction::load_i32(register, value));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn load_undefined(&mut self) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        self.instructions
            .push(Instruction::load_undefined(register));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn load_null(&mut self) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        self.instructions.push(Instruction::load_null(register));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn emit_new_closure(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
    ) -> Result<(), SourceLoweringError> {
        let captures = if explicit_captures.is_empty() {
            self.captures.clone()
        } else {
            explicit_captures.to_vec()
        };

        let capture_count = RegisterIndex::try_from(captures.len())
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let capture_start = if capture_count == 0 {
            BytecodeRegister::new(self.next_local + self.next_temp)
        } else {
            self.reserve_temp_window(capture_count)?
        };

        for (offset, capture) in captures.iter().enumerate() {
            let register = BytecodeRegister::new(capture_start.index() + offset as u16);
            match capture {
                CaptureSource::Register(source) => {
                    if *source != register {
                        self.instructions
                            .push(Instruction::move_(register, *source));
                    }
                }
                CaptureSource::Upvalue(upvalue) => {
                    self.instructions
                        .push(Instruction::get_upvalue(register, *upvalue));
                }
            }
        }

        let pc = self.instructions.len();
        self.instructions
            .push(Instruction::new_closure(destination, capture_start));
        self.record_closure_template(pc, ClosureTemplate::new(callee, capture_count));

        if capture_count != 0 {
            self.release_temp_window(capture_count);
        }

        Ok(())
    }

    pub(super) fn assign_to_name(
        &mut self,
        name: &str,
        value: ValueLocation,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match self.resolve_binding(name)? {
            Binding::Register(register) => {
                if register != value.register {
                    self.instructions
                        .push(Instruction::move_(register, value.register));
                    self.release(value);
                }
                Ok(ValueLocation::local(register))
            }
            Binding::Function {
                closure_register, ..
            } => {
                if closure_register != value.register {
                    self.instructions
                        .push(Instruction::move_(closure_register, value.register));
                    self.release(value);
                }
                Ok(ValueLocation::local(closure_register))
            }
            Binding::Upvalue(upvalue) => {
                self.instructions
                    .push(Instruction::set_upvalue(value.register, upvalue));
                Ok(value)
            }
        }
    }

    pub(super) fn assign_binding(
        &mut self,
        name: &str,
        register: BytecodeRegister,
        value: ValueLocation,
    ) -> Result<(), SourceLoweringError> {
        let assigned = self.assign_to_name(name, value)?;
        if assigned.register != register {
            self.instructions
                .push(Instruction::move_(register, assigned.register));
        }
        Ok(())
    }

    pub(super) fn declare_variable_binding(
        &mut self,
        name: &str,
        allow_redeclare: bool,
    ) -> Result<BytecodeRegister, SourceLoweringError> {
        if let Some(existing) = self.env.bindings.get(name).copied() {
            return match existing {
                Binding::Register(register) => {
                    if allow_redeclare {
                        Ok(register)
                    } else {
                        Err(SourceLoweringError::DuplicateBinding(name.to_string()))
                    }
                }
                Binding::Function {
                    closure_register, ..
                } => {
                    if allow_redeclare {
                        Ok(closure_register)
                    } else {
                        Err(SourceLoweringError::DuplicateBinding(name.to_string()))
                    }
                }
                Binding::Upvalue(_) => Err(SourceLoweringError::DuplicateBinding(name.to_string())),
            };
        }

        let register = self.allocate_local()?;
        self.env
            .bindings
            .insert(name.to_string(), Binding::Register(register));
        Ok(register)
    }

    pub(super) fn declare_function_binding(
        &mut self,
        name: &str,
    ) -> Result<BytecodeRegister, SourceLoweringError> {
        let closure_register = if let Some(existing) = self.env.bindings.get(name).copied() {
            match existing {
                Binding::Register(register) => register,
                Binding::Function {
                    closure_register, ..
                } => closure_register,
                Binding::Upvalue(_) => {
                    return Err(SourceLoweringError::Unsupported(format!(
                        "function declaration {name} conflicts with an upvalue binding"
                    )));
                }
            }
        } else {
            self.allocate_local()?
        };

        self.env
            .bindings
            .insert(name.to_string(), Binding::Function { closure_register });
        Ok(closure_register)
    }

    pub(super) fn resolve_binding(&mut self, name: &str) -> Result<Binding, SourceLoweringError> {
        if let Some(binding) = self.env.bindings.get(name).copied() {
            return Ok(binding);
        }

        if let Some(parent_env) = &self.parent_env
            && let Some(binding) = parent_env.bindings.get(name).copied()
        {
            let upvalue = if let Some(existing) = self.capture_ids.get(name).copied() {
                existing
            } else {
                let upvalue = UpvalueId(
                    u16::try_from(self.captures.len())
                        .map_err(|_| SourceLoweringError::TooManyLocals)?,
                );
                self.captures.push(binding.capture_source());
                self.capture_ids.insert(name.to_string(), upvalue);
                upvalue
            };
            self.env
                .bindings
                .insert(name.to_string(), Binding::Upvalue(upvalue));
            return Ok(Binding::Upvalue(upvalue));
        }

        Err(SourceLoweringError::UnknownBinding(name.to_string()))
    }

    pub(super) fn allocate_local(&mut self) -> Result<BytecodeRegister, SourceLoweringError> {
        let register = BytecodeRegister::new(self.next_local);
        self.next_local = self
            .next_local
            .checked_add(1)
            .ok_or(SourceLoweringError::TooManyLocals)?;
        Ok(register)
    }

    pub(super) fn alloc_temp(&mut self) -> BytecodeRegister {
        let register = BytecodeRegister::new(self.next_local + self.next_temp);
        self.next_temp = self.next_temp.saturating_add(1);
        self.max_temp = self.max_temp.max(self.next_temp);
        register
    }

    pub(super) fn reserve_temp_window(
        &mut self,
        size: RegisterIndex,
    ) -> Result<BytecodeRegister, SourceLoweringError> {
        let start = BytecodeRegister::new(self.next_local + self.next_temp);
        self.next_temp = self
            .next_temp
            .checked_add(size)
            .ok_or(SourceLoweringError::TooManyLocals)?;
        self.max_temp = self.max_temp.max(self.next_temp);
        Ok(start)
    }

    pub(super) fn release_temp_window(&mut self, size: RegisterIndex) {
        self.next_temp = self.next_temp.saturating_sub(size);
    }

    pub(super) fn release(&mut self, value: ValueLocation) {
        if value.is_temp {
            self.next_temp = self.next_temp.saturating_sub(1);
        }
    }

    fn declare_intrinsic_global_binding(&mut self, name: &str) -> Result<(), SourceLoweringError> {
        if self.env.bindings.contains_key(name) {
            return Ok(());
        }

        let global = self.alloc_temp();
        self.instructions.push(Instruction::load_this(global));
        let binding = self.allocate_local()?;
        let property = self.intern_property_name(name)?;
        self.instructions
            .push(Instruction::get_property(binding, global, property));
        self.release(ValueLocation::temp(global));
        self.env
            .bindings
            .insert(name.to_string(), Binding::Register(binding));
        Ok(())
    }

    pub(super) fn materialize_value(&mut self, value: ValueLocation) -> ValueLocation {
        if value.is_temp {
            return value;
        }

        let register = self.alloc_temp();
        self.instructions
            .push(Instruction::move_(register, value.register));
        ValueLocation::temp(register)
    }

    fn emit_iterator_closes_for_active_loops(&mut self) {
        for loop_scope in self.loop_stack.iter().rev() {
            if let Some(iterator_register) = loop_scope.iterator_register {
                self.instructions
                    .push(Instruction::iterator_close(iterator_register));
            }
        }
    }

    fn patch_loop_scope(
        &mut self,
        loop_scope: LoopScope,
        break_target: usize,
        default_continue_target: usize,
    ) -> Result<(), SourceLoweringError> {
        for jump in loop_scope.break_jumps {
            self.patch_jump(jump, break_target)?;
        }
        let continue_target = loop_scope
            .continue_target
            .unwrap_or(default_continue_target);
        for jump in loop_scope.continue_jumps {
            self.patch_jump(jump, continue_target)?;
        }
        Ok(())
    }

    pub(super) fn emit_implicit_return(&mut self) -> Result<(), SourceLoweringError> {
        let value = match self.kind {
            FunctionKind::Script => match self.mode {
                LoweringMode::Script => self.load_undefined()?,
                LoweringMode::Test262Basic => self.load_i32(0)?,
            },
            FunctionKind::Ordinary => self.load_undefined()?,
        };
        self.instructions.push(Instruction::ret(value.register));
        self.release(value);
        Ok(())
    }

    pub(super) fn intern_property_name(
        &mut self,
        name: &str,
    ) -> Result<PropertyNameId, SourceLoweringError> {
        if let Some(existing) = self.property_name_ids.get(name).copied() {
            return Ok(existing);
        }

        let id = PropertyNameId(
            u16::try_from(self.property_names.len())
                .map_err(|_| SourceLoweringError::TooManyLocals)?,
        );
        self.property_names.push(name.to_string().into_boxed_str());
        self.property_name_ids.insert(name.to_string(), id);
        Ok(id)
    }

    #[allow(dead_code)]
    pub(super) fn intern_string(&mut self, value: &str) -> Result<StringId, SourceLoweringError> {
        if let Some(existing) = self.string_ids.get(value).copied() {
            return Ok(existing);
        }

        let id = StringId(
            u16::try_from(self.string_literals.len())
                .map_err(|_| SourceLoweringError::TooManyLocals)?,
        );
        self.string_literals
            .push(value.to_string().into_boxed_str());
        self.string_ids.insert(value.to_string(), id);
        Ok(id)
    }

    pub(super) fn record_closure_template(&mut self, pc: usize, template: ClosureTemplate) {
        ensure_side_table_len(&mut self.closure_templates, pc);
        self.closure_templates[pc] = Some(template);
    }

    pub(super) fn record_call_site(&mut self, pc: usize, call_site: CallSite) {
        ensure_side_table_len(&mut self.call_sites, pc);
        self.call_sites[pc] = Some(call_site);
    }

    pub(super) fn emit_jump_placeholder(&mut self) -> usize {
        let index = self.instructions.len();
        self.instructions
            .push(Instruction::jump(JumpOffset::new(0)));
        index
    }

    pub(super) fn emit_conditional_placeholder(
        &mut self,
        opcode: Opcode,
        cond: BytecodeRegister,
    ) -> usize {
        let index = self.instructions.len();
        let instruction = match opcode {
            Opcode::JumpIfTrue => Instruction::jump_if_true(cond, JumpOffset::new(0)),
            Opcode::JumpIfFalse => Instruction::jump_if_false(cond, JumpOffset::new(0)),
            _ => panic!("conditional placeholder requires a conditional jump"),
        };
        self.instructions.push(instruction);
        index
    }

    pub(super) fn emit_relative_jump(&mut self, target: usize) -> Result<(), SourceLoweringError> {
        let source = self.instructions.len();
        let offset = compute_offset(source, target)?;
        self.instructions.push(Instruction::jump(offset));
        Ok(())
    }

    pub(super) fn patch_jump(
        &mut self,
        source: usize,
        target: usize,
    ) -> Result<(), SourceLoweringError> {
        let offset = compute_offset(source, target)?;
        let existing = self.instructions[source];
        self.instructions[source] = match existing.opcode() {
            Opcode::Jump => Instruction::jump(offset),
            Opcode::JumpIfTrue => {
                Instruction::jump_if_true(BytecodeRegister::new(existing.a()), offset)
            }
            Opcode::JumpIfFalse => {
                Instruction::jump_if_false(BytecodeRegister::new(existing.a()), offset)
            }
            _ => {
                return Err(SourceLoweringError::Unsupported(
                    "attempted to patch a non-jump instruction".to_string(),
                ));
            }
        };
        Ok(())
    }
}

fn ensure_side_table_len<T>(table: &mut Vec<Option<T>>, index: usize) {
    if table.len() <= index {
        table.resize_with(index + 1, || None);
    }
}

fn compute_offset(source: usize, target: usize) -> Result<JumpOffset, SourceLoweringError> {
    let source = i64::try_from(source).map_err(|_| {
        SourceLoweringError::Unsupported("jump source exceeded bytecode range".to_string())
    })?;
    let target = i64::try_from(target).map_err(|_| {
        SourceLoweringError::Unsupported("jump target exceeded bytecode range".to_string())
    })?;
    let offset = i32::try_from(target - source - 1).map_err(|_| {
        SourceLoweringError::Unsupported("jump offset exceeded bytecode range".to_string())
    })?;
    Ok(JumpOffset::new(offset))
}
