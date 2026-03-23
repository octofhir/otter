use super::ast::{
    collect_function_declarations, collect_var_names, extract_function_params,
    is_test262_failure_throw,
};
use super::module_compiler::ModuleCompiler;
use super::shared::{
    Binding, CaptureSource, CompileEnv, CompiledFunction, FunctionCompiler, FunctionKind,
    PendingFunction, ValueLocation,
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
            closure_templates: Vec::new(),
            call_sites: Vec::new(),
            captures: Vec::new(),
            capture_ids: BTreeMap::new(),
            hoisted_functions: Vec::new(),
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
            let closure_register =
                self.declare_function_binding(name.name.as_str(), reserved_index)?;
            let pending = PendingFunction {
                reserved: reserved_index,
                closure_register,
                captures: Vec::new(),
            };

            let params = extract_function_params(function)?;
            let compiled = module.compile_function_from_statements(
                reserved_index,
                Some(name.name.to_string()),
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
            0,
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
            ExceptionTable::default(),
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
                let _ = self.compile_statement(&while_statement.body, module)?;
                self.emit_relative_jump(loop_start)?;
                self.patch_jump(exit_jump, self.instructions.len())?;
                Ok(false)
            }
            AstStatement::DoWhileStatement(do_while_statement) => {
                let loop_start = self.instructions.len();
                if self.compile_statement(&do_while_statement.body, module)? {
                    return Ok(true);
                }

                let condition = self.compile_expression(&do_while_statement.test, module)?;
                let back_jump =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, condition.register);
                self.release(condition);
                self.patch_jump(back_jump, loop_start)?;
                Ok(false)
            }
            AstStatement::ReturnStatement(return_statement) => {
                let value = if let Some(argument) = &return_statement.argument {
                    self.compile_expression(argument, module)?
                } else {
                    self.load_undefined()?
                };
                self.instructions.push(Instruction::ret(value.register));
                self.release(value);
                Ok(true)
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
        function: FunctionIndex,
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

        self.env.bindings.insert(
            name.to_string(),
            Binding::Function {
                function,
                closure_register,
            },
        );
        Ok(closure_register)
    }

    pub(super) fn resolve_binding(&mut self, name: &str) -> Result<Binding, SourceLoweringError> {
        if let Some(binding) = self.env.bindings.get(name).copied() {
            return Ok(binding);
        }

        if let Some(parent_env) = &self.parent_env
            && let Some(binding) = parent_env.bindings.get(name).copied()
        {
            if let Binding::Function { .. } = binding {
                self.env.bindings.insert(name.to_string(), binding);
                return Ok(binding);
            }

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

    pub(super) fn materialize_value(&mut self, value: ValueLocation) -> ValueLocation {
        if value.is_temp {
            return value;
        }

        let register = self.alloc_temp();
        self.instructions
            .push(Instruction::move_(register, value.register));
        ValueLocation::temp(register)
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
