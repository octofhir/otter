use super::ast::{
    ParamInfo, collect_binding_identifier_names, collect_function_declarations, collect_var_names,
    expected_function_length, extract_function_params, identifier_name_for_parameter_pattern,
    is_test262_failure_throw,
};
use super::module_compiler::{FunctionIdentity, ModuleCompiler};
use super::shared::{
    Binding, CaptureSource, CompileEnv, CompiledFunction, FunctionCompiler, FunctionKind,
    LoopScope, PendingFunction, ValueLocation,
};
use super::*;

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
            float_constants: Vec::new(),
            closure_templates: Vec::new(),
            call_sites: Vec::new(),
            exception_handlers: Vec::new(),
            captures: Vec::new(),
            capture_ids: BTreeMap::new(),
            hoisted_functions: Vec::new(),
            finally_stack: Vec::new(),
            loop_stack: Vec::new(),
            pending_loop_label: None,
            arguments_local: None,
            rest_local: None,
            parameter_binding_registers: Vec::new(),
            parameter_tdz_active: false,
            _marker: std::marker::PhantomData,
        }
    }

    fn declare_parameter_pattern_bindings(
        &mut self,
        pattern: &BindingPattern<'_>,
    ) -> Result<(), SourceLoweringError> {
        let mut names = Vec::new();
        collect_binding_identifier_names(pattern, &mut names);
        for name in names {
            let register = self.allocate_local()?;
            self.env.bindings.insert(name, Binding::Register(register));
            self.parameter_binding_registers.push(register);
        }
        Ok(())
    }

    pub(super) fn declare_parameters(
        &mut self,
        params: &[ParamInfo<'_>],
    ) -> Result<(), SourceLoweringError> {
        for param in params {
            if param.is_rest {
                let register = self.allocate_local()?;
                if let Some(name) = identifier_name_for_parameter_pattern(param.pattern) {
                    self.env
                        .bindings
                        .insert(name.to_string(), Binding::Register(register));
                    self.parameter_binding_registers.push(register);
                } else {
                    self.declare_parameter_pattern_bindings(param.pattern)?;
                }
                self.rest_local = Some(register);
                continue;
            }
            let register = BytecodeRegister::new(self.parameter_count);
            self.parameter_count = self
                .parameter_count
                .checked_add(1)
                .ok_or(SourceLoweringError::TooManyLocals)?;
            self.next_local = self.parameter_count;
            if let Some(name) = identifier_name_for_parameter_pattern(param.pattern) {
                self.env
                    .bindings
                    .insert(name.to_string(), Binding::Register(register));
                self.parameter_binding_registers.push(register);
            } else {
                self.declare_parameter_pattern_bindings(param.pattern)?;
            }
        }
        Ok(())
    }

    /// Allocates a local for `this` and emits LoadThis, binding it as "this".
    pub(super) fn declare_this_binding(&mut self) -> Result<(), SourceLoweringError> {
        let register = self.allocate_local()?;
        self.instructions.push(Instruction::load_this(register));
        self.env
            .bindings
            .insert("this".to_string(), Binding::Register(register));
        Ok(())
    }

    /// Emits default-parameter and destructuring initialization left-to-right.
    pub(super) fn compile_parameter_initialization(
        &mut self,
        params: &[ParamInfo<'_>],
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let mut incoming_param_registers = Vec::new();
        let mut parameter_index: RegisterIndex = 0;
        for param in params {
            if param.is_rest {
                continue;
            }
            let incoming = self.allocate_local()?;
            self.instructions.push(Instruction::move_(
                incoming,
                BytecodeRegister::new(parameter_index),
            ));
            incoming_param_registers.push(incoming);
            parameter_index = parameter_index
                .checked_add(1)
                .ok_or(SourceLoweringError::TooManyLocals)?;
        }
        self.parameter_tdz_active = true;
        for &register in &self.parameter_binding_registers {
            self.instructions.push(Instruction::load_hole(register));
        }

        let mut incoming_index = 0usize;
        let mut target_param_index: RegisterIndex = 0;
        for param in params {
            if param.is_rest {
                let register = self.rest_local.ok_or_else(|| {
                    SourceLoweringError::Unsupported(
                        "rest parameter local was not allocated".to_string(),
                    )
                })?;
                self.instructions
                    .push(Instruction::create_rest_parameters(register));
                if !matches!(param.pattern, BindingPattern::BindingIdentifier(_)) {
                    self.compile_binding_pattern_target(
                        param.pattern,
                        ValueLocation::local(register),
                        false,
                        module,
                    )?;
                }
                continue;
            }
            let incoming = *incoming_param_registers
                .get(incoming_index)
                .ok_or_else(|| {
                    SourceLoweringError::Unsupported("missing incoming parameter".to_string())
                })?;
            incoming_index += 1;

            let register = BytecodeRegister::new(target_param_index);
            target_param_index = target_param_index
                .checked_add(1)
                .ok_or(SourceLoweringError::TooManyLocals)?;

            let source = ValueLocation::local(incoming);
            if let Some(default_expr) = param.default {
                let undef = self.load_undefined()?;
                let cmp = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::eq(
                    cmp.register,
                    source.register,
                    undef.register,
                ));
                self.release(undef);
                let use_actual_jump =
                    self.emit_conditional_placeholder(Opcode::JumpIfFalse, cmp.register);
                self.release(cmp);

                let default_val = self.compile_expression_with_inferred_name(
                    default_expr,
                    identifier_name_for_parameter_pattern(param.pattern),
                    module,
                )?;
                if default_val.register != register {
                    self.instructions
                        .push(Instruction::move_(register, default_val.register));
                    self.release(default_val);
                }
                let done_jump = self.emit_jump_placeholder();
                self.patch_jump(use_actual_jump, self.instructions.len())?;
                self.instructions
                    .push(Instruction::move_(register, source.register));
                self.patch_jump(done_jump, self.instructions.len())?;
            } else {
                self.instructions
                    .push(Instruction::move_(register, source.register));
            }

            if !matches!(param.pattern, BindingPattern::BindingIdentifier(_)) {
                self.compile_binding_pattern_target(
                    param.pattern,
                    ValueLocation::local(register),
                    false,
                    module,
                )?;
            }
        }
        self.parameter_tdz_active = false;
        Ok(())
    }

    pub(super) fn declare_intrinsic_globals(&mut self) -> Result<(), SourceLoweringError> {
        for name in crate::intrinsics::CORE_INTRINSIC_GLOBAL_NAMES {
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
                    length: expected_function_length(&params),
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
        length: u16,
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
                FloatTable::new(self.float_constants),
                ClosureTable::new(self.closure_templates),
                CallTable::new(self.call_sites),
            ),
            FeedbackTableLayout::default(),
            DeoptTable::default(),
            ExceptionTable::new(self.exception_handlers),
            SourceMap::default(),
        );

        Ok(CompiledFunction {
            function: VmFunction::new_with_length(
                name,
                length,
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
                    label: self.pending_loop_label.take(),
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
                    label: self.pending_loop_label.take(),
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
            AstStatement::ForStatement(for_statement) => {
                self.compile_for_statement(for_statement, module)
            }
            AstStatement::ForOfStatement(for_of_statement) => {
                self.compile_for_of_statement(for_of_statement, module)
            }
            AstStatement::ForInStatement(for_in_statement) => {
                self.compile_for_in_statement(for_in_statement, module)
            }
            AstStatement::LabeledStatement(labeled) => {
                self.compile_labeled_statement(labeled, module)
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
            AstStatement::SwitchStatement(switch) => self.compile_switch_statement(switch, module),
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
        let is_var = declaration.kind == VariableDeclarationKind::Var;

        for declarator in &declaration.declarations {
            match &declarator.id {
                BindingPattern::BindingIdentifier(identifier) => {
                    let register =
                        self.declare_variable_binding(identifier.name.as_str(), is_var)?;
                    if let Some(init) = &declarator.init {
                        let value = self.compile_expression_with_inferred_name(
                            init,
                            Some(identifier.name.as_str()),
                            module,
                        )?;
                        self.assign_binding(identifier.name.as_str(), register, value)?;
                    }
                }
                BindingPattern::ObjectPattern(_) | BindingPattern::ArrayPattern(_) => {
                    let init = declarator.init.as_ref().ok_or_else(|| {
                        SourceLoweringError::Unsupported(
                            "destructuring declaration requires an initializer".to_string(),
                        )
                    })?;
                    let value = self.compile_expression(init, module)?;
                    let materialized = self.materialize_value(value);
                    self.compile_binding_pattern_target(
                        &declarator.id,
                        materialized,
                        false,
                        module,
                    )?;
                }
                BindingPattern::AssignmentPattern(_) => {
                    return Err(SourceLoweringError::Unsupported(
                        "assignment pattern as variable declaration binding".to_string(),
                    ));
                }
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

    pub(super) fn load_bool_into_register(
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

    pub(super) fn load_nan(&mut self) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        self.instructions.push(Instruction::load_nan(register));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn load_undefined(&mut self) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        self.instructions
            .push(Instruction::load_undefined(register));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn emit_assert_not_hole(&mut self, register: BytecodeRegister) {
        self.instructions
            .push(Instruction::assert_not_hole(register));
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
        self.emit_new_closure_with_flags(
            destination,
            callee,
            explicit_captures,
            crate::object::ClosureFlags::normal(),
        )
    }

    pub(super) fn emit_new_closure_arrow(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
    ) -> Result<(), SourceLoweringError> {
        self.emit_new_closure_with_flags(
            destination,
            callee,
            explicit_captures,
            crate::object::ClosureFlags::arrow(),
        )
    }

    fn emit_new_closure_with_flags(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
        closure_flags: crate::object::ClosureFlags,
    ) -> Result<(), SourceLoweringError> {
        let captures = if explicit_captures.is_empty() {
            self.captures.clone()
        } else {
            explicit_captures.to_vec()
        };

        let pc = self.instructions.len();
        self.instructions.push(Instruction::new_closure(
            destination,
            BytecodeRegister::new(0),
        ));
        self.record_closure_template(
            pc,
            ClosureTemplate::with_flags(callee, captures, closure_flags),
        );

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

        // ES2024 §10.4.4: `arguments` is implicitly available in non-arrow functions.
        // Lazily allocate a local and emit CreateArguments on first access.
        if name == "arguments" && self.kind != FunctionKind::Arrow {
            let register = if let Some(reg) = self.arguments_local {
                reg
            } else {
                let reg = self.allocate_local()?;
                self.instructions.push(Instruction::create_arguments(reg));
                self.arguments_local = Some(reg);
                self.env
                    .bindings
                    .insert("arguments".to_string(), Binding::Register(reg));
                reg
            };
            return Ok(Binding::Register(register));
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

    pub(super) fn stabilize_binding_value(
        &mut self,
        value: ValueLocation,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.allocate_local()?;
        if value.register != register {
            self.instructions
                .push(Instruction::move_(register, value.register));
        }
        self.release(value);
        Ok(ValueLocation::local(register))
    }

    pub(super) fn emit_iterator_closes_for_active_loops(&mut self) {
        for loop_scope in self.loop_stack.iter().rev() {
            if let Some(iterator_register) = loop_scope.iterator_register {
                self.instructions
                    .push(Instruction::iterator_close(iterator_register));
            }
        }
    }

    pub(super) fn patch_loop_scope(
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
            FunctionKind::Ordinary | FunctionKind::Arrow => self.load_undefined()?,
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
