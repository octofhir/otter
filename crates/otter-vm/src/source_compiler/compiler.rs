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
            strict_mode: false,
            is_derived_constructor: false,
            has_instance_fields: false,
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
            bigint_constants: Vec::new(),
            bigint_ids: BTreeMap::new(),
            regexp_literals: Vec::new(),
            regexp_ids: BTreeMap::new(),
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
            eval_completion_register: None,
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

    /// Allocates the lexical `this` binding slot used by nested arrow functions.
    ///
    /// Derived constructors start with an uninitialized `this` binding that is
    /// only initialized after `super()` completes.
    pub(super) fn declare_this_binding(&mut self) -> Result<(), SourceLoweringError> {
        let register = self.allocate_local()?;
        if self.is_derived_constructor {
            self.instructions.push(Instruction::load_hole(register));
        } else {
            self.instructions.push(Instruction::load_this(register));
        }
        self.env
            .bindings
            .insert("this".to_string(), Binding::ThisRegister(register));
        Ok(())
    }

    /// Reserves a stable local slot for the implicit `arguments` binding.
    ///
    /// The actual arguments object remains lazily materialized on first access,
    /// but the slot itself must exist before any temp registers are allocated.
    pub(super) fn reserve_arguments_binding_slot(&mut self) -> Result<(), SourceLoweringError> {
        if self.kind == FunctionKind::Arrow || self.arguments_local.is_some() {
            return Ok(());
        }
        let register = self.allocate_local()?;
        self.arguments_local = Some(register);
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
        let mut pending_functions = Vec::with_capacity(reserved.len());
        for function in &reserved {
            let name = function.id.as_ref().ok_or_else(|| {
                SourceLoweringError::Unsupported(
                    "function declarations without identifiers".to_string(),
                )
            })?;

            let reserved_index = module.reserve_function();
            let closure_register = self.declare_function_binding(name.name.as_str())?;
            pending_functions.push((
                *function,
                PendingFunction {
                    reserved: reserved_index,
                    closure_register,
                    captures: Vec::new(),
                    is_generator: function.generator,
                    is_async: function.r#async,
                },
            ));
        }

        for (function, pending) in pending_functions {
            let name = function.id.as_ref().ok_or_else(|| {
                SourceLoweringError::Unsupported(
                    "function declarations without identifiers".to_string(),
                )
            })?;

            let compiled = module.compile_function_from_statements(
                pending.reserved,
                FunctionIdentity {
                    debug_name: Some(name.name.to_string()),
                    self_binding_name: Some(name.name.to_string()),
                    length: expected_function_length(&extract_function_params(function)?),
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
                &extract_function_params(function)?,
                if function.generator && function.r#async {
                    FunctionKind::AsyncGenerator
                } else if function.generator {
                    FunctionKind::Generator
                } else if function.r#async {
                    FunctionKind::Async
                } else {
                    FunctionKind::Ordinary
                },
                Some(self.env.clone()),
                self.strict_mode
                    || super::ast::has_use_strict_directive(
                        function
                            .body
                            .as_ref()
                            .map(|body| body.directives.as_slice())
                            .unwrap_or(&[]),
                    ),
            )?;
            module.set_function(pending.reserved, compiled.function);
            self.hoisted_functions.push(PendingFunction {
                captures: compiled.captures,
                reserved: pending.reserved,
                closure_register: pending.closure_register,
                is_generator: function.generator,
                is_async: function.r#async,
            });
        }

        Ok(())
    }

    pub(super) fn emit_hoisted_function_initializers(&mut self) -> Result<(), SourceLoweringError> {
        for pending in self.hoisted_functions.clone() {
            if pending.is_generator && pending.is_async {
                self.emit_new_closure_async_generator(
                    pending.closure_register,
                    pending.reserved,
                    &pending.captures,
                )?;
            } else if pending.is_generator {
                self.emit_new_closure_generator(
                    pending.closure_register,
                    pending.reserved,
                    &pending.captures,
                )?;
            } else if pending.is_async {
                self.emit_new_closure_async(
                    pending.closure_register,
                    pending.reserved,
                    &pending.captures,
                )?;
            } else {
                self.emit_new_closure(
                    pending.closure_register,
                    pending.reserved,
                    &pending.captures,
                )?;
            }
            self.mirror_script_binding_to_global_by_register(pending.closure_register)?;
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
                crate::bigint::BigIntTable::new(self.bigint_constants),
                ClosureTable::new(self.closure_templates),
                CallTable::new(self.call_sites),
                crate::regexp::RegExpTable::new(self.regexp_literals),
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
            )
            .with_strict(self.strict_mode)
            .with_derived_constructor(self.is_derived_constructor)
            .with_generator(self.kind.is_generator())
            .with_async(self.kind.is_async()),
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
            AstStatement::ClassDeclaration(class) => {
                self.compile_class_declaration(class, module)?;
                Ok(false)
            }
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
                        if is_var {
                            self.mirror_script_binding_to_global(
                                identifier.name.as_str(),
                                register,
                            )?;
                        }
                    } else if is_var && self.kind == FunctionKind::Script {
                        let undefined = self.load_undefined()?;
                        if undefined.register != register {
                            self.instructions
                                .push(Instruction::move_(register, undefined.register));
                        }
                        self.release(undefined);
                        self.mirror_script_binding_to_global(identifier.name.as_str(), register)?;
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

    /// §15.7 ClassDeclaration — `class Name { ... }`
    /// Spec: <https://tc39.es/ecma262/#sec-class-definitions>
    fn compile_class_declaration(
        &mut self,
        class: &Class<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let name = class.id.as_ref().ok_or_else(|| {
            SourceLoweringError::Unsupported("class declarations without identifiers".to_string())
        })?;
        let binding = self.declare_variable_binding(name.name.as_str(), false)?;

        let constructor_value = self.compile_class_body(class, name.name.as_str(), module)?;

        if constructor_value.register != binding {
            self.instructions
                .push(Instruction::move_(binding, constructor_value.register));
        }

        Ok(())
    }

    /// §15.7.14 ClassDefinitionEvaluation — shared implementation for class
    /// declarations and class expressions.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    pub(super) fn compile_class_body(
        &mut self,
        class: &Class<'_>,
        class_name: &str,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        use oxc_ast::ast::{ClassElement, MethodDefinitionKind, PropertyDefinitionType};

        // ── First pass: extract constructor, count instance fields, detect private members ──
        let mut constructor = None;
        let mut has_instance_fields = false;
        let mut has_private_members = false;
        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(method)
                    if matches!(method.kind, MethodDefinitionKind::Constructor) =>
                {
                    if constructor.is_some() {
                        return Err(SourceLoweringError::Unsupported(
                            "duplicate class constructors".to_string(),
                        ));
                    }
                    if method.r#static {
                        return Err(SourceLoweringError::Unsupported(
                            "static class constructors".to_string(),
                        ));
                    }
                    constructor = Some(&method.value);
                }
                ClassElement::MethodDefinition(method) => {
                    if matches!(
                        &method.key,
                        oxc_ast::ast::PropertyKey::PrivateIdentifier(_)
                    ) {
                        has_private_members = true;
                    }
                }
                ClassElement::PropertyDefinition(prop) => {
                    if prop.declare {
                        continue;
                    }
                    if matches!(
                        prop.r#type,
                        PropertyDefinitionType::TSAbstractPropertyDefinition
                    ) {
                        continue;
                    }
                    if !prop.r#static {
                        has_instance_fields = true;
                    }
                    if matches!(
                        &prop.key,
                        oxc_ast::ast::PropertyKey::PrivateIdentifier(_)
                    ) {
                        has_private_members = true;
                    }
                }
                ClassElement::StaticBlock(_) => {}
                ClassElement::AccessorProperty(_) => {
                    return Err(SourceLoweringError::Unsupported(
                        "accessor class properties (auto-accessor) are not yet implemented"
                            .to_string(),
                    ));
                }
                _ => {
                    return Err(SourceLoweringError::Unsupported(
                        "unsupported class element".to_string(),
                    ));
                }
            }
        }

        // ── Compile super class ─────────────────────────────────────────────
        // §15.7.14 step 5: Detect `class extends null` — protoParent = null,
        // constructorParent = %Function.prototype%, constructor kind = base.
        // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
        let extends_null = matches!(
            class.super_class.as_ref(),
            Some(Expression::NullLiteral(_))
        );
        let super_class = if let Some(super_class) = class.super_class.as_ref() {
            if extends_null {
                None // Don't compile null as a super class value
            } else {
                let super_value = self.compile_expression(super_class, module)?;
                Some(self.stabilize_binding_value(super_value)?)
            }
        } else {
            None
        };
        let is_derived = super_class.is_some() && !extends_null;

        // ── Compile constructor ─────────────────────────────────────────────
        // RunClassFieldInitializer is needed both for instance fields AND for
        // copying private methods/accessors to instances.
        let needs_field_initializer = has_instance_fields || has_private_members;
        let constructor_value = if let Some(ctor) = constructor {
            self.compile_class_constructor_with_fields(
                class_name,
                ctor,
                is_derived,
                needs_field_initializer,
                module,
            )?
        } else if is_derived {
            self.compile_default_derived_class_constructor_with_fields(
                class_name,
                needs_field_initializer,
                module,
            )?
        } else {
            self.compile_default_base_class_constructor_with_fields(
                class_name,
                needs_field_initializer,
                module,
            )?
        };
        let constructor_value = if constructor_value.is_temp {
            self.stabilize_binding_value(constructor_value)?
        } else {
            constructor_value
        };

        // ── Set up prototype chain ──────────────────────────────────────────
        if let Some(super_class) = super_class {
            // Normal extends: constructor.__proto__ = superClass
            self.emit_object_method_call(
                "setPrototypeOf",
                constructor_value,
                &[super_class],
                module,
            )?;
        }
        // For extends null: constructor.__proto__ stays as Function.prototype (default).

        let prototype = self.emit_named_property_load(constructor_value, "prototype")?;
        let prototype = self.stabilize_binding_value(prototype)?;
        let prototype_parent = if extends_null {
            // §15.7.14 step 5.b.i: protoParent = null
            // Stabilize to prevent clobbering by internal allocations in
            // emit_object_method_call.
            let null_val = self.load_null()?;
            self.stabilize_binding_value(null_val)?
        } else if let Some(super_class) = super_class {
            let parent = self.emit_named_property_load(super_class, "prototype")?;
            self.stabilize_binding_value(parent)?
        } else {
            let object_ctor = self.compile_identifier("Object")?;
            let object_ctor = if object_ctor.is_temp {
                self.stabilize_binding_value(object_ctor)?
            } else {
                object_ctor
            };
            let parent = self.emit_named_property_load(object_ctor, "prototype")?;
            self.stabilize_binding_value(parent)?
        };
        self.emit_object_method_call("setPrototypeOf", prototype, &[prototype_parent], module)?;
        self.release(prototype_parent);

        // ── AllocClassId if the class has private members ──────────────────
        // §6.2.12 — Allocate a unique class identifier for private name resolution.
        if has_private_members {
            self.instructions
                .push(Instruction::alloc_class_id(constructor_value.register));
        }

        // ── Second pass: install methods ────────────────────────────────────
        // §15.7.14 ClassDefinitionEvaluation step 26–28.
        for element in &class.body.body {
            if let ClassElement::MethodDefinition(method) = element
                && !matches!(method.kind, MethodDefinitionKind::Constructor)
            {
                let is_private = matches!(
                    &method.key,
                    oxc_ast::ast::PropertyKey::PrivateIdentifier(_)
                );
                let class_id_src = if has_private_members {
                    Some(constructor_value.register)
                } else {
                    None
                };
                if is_private {
                    self.compile_private_class_method(
                        method,
                        constructor_value,
                        prototype,
                        module,
                    )?;
                } else {
                    let target = if method.r#static {
                        constructor_value
                    } else {
                        prototype
                    };
                    self.compile_class_method(method, target, class_id_src, module)?;
                }
            }
        }

        self.emit_make_class_prototype_non_writable(constructor_value, module)?;

        // ── Compile instance field initializer ──────────────────────────────
        // §15.7.14 step 29: Create an initializer function for instance fields.
        if needs_field_initializer {
            self.compile_class_field_initializer(
                class,
                constructor_value,
                has_private_members,
                module,
            )?;
        }

        // ── Third pass: static fields and static blocks ─────────────────────
        // §15.7.14 step 34: Evaluate static field initializers in order.
        // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
        for element in &class.body.body {
            match element {
                ClassElement::PropertyDefinition(prop) if prop.r#static && !prop.declare => {
                    if let oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) = &prop.key {
                        // Static private field: compile value and emit DefinePrivateField
                        // on the constructor.
                        self.compile_static_private_field(
                            ident.name.as_str(),
                            prop,
                            constructor_value,
                            module,
                        )?;
                    } else {
                        self.compile_static_field(prop, constructor_value, module)?;
                    }
                }
                ClassElement::StaticBlock(block) => {
                    self.compile_static_block(block, constructor_value, module)?;
                }
                _ => {} // methods & instance fields handled above
            }
        }

        Ok(constructor_value)
    }

    /// §15.7.14 step 29 — Compile a synthetic function that initializes instance
    /// fields and store it on the constructor via SetClassFieldInitializer.
    /// Spec: <https://tc39.es/ecma262/#sec-definefield>
    fn compile_class_field_initializer(
        &mut self,
        class: &Class<'_>,
        constructor_value: ValueLocation,
        has_private_members: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::non_computed_property_key_name;
        use oxc_ast::ast::{ClassElement, PropertyDefinitionType};

        // Compile the synthetic initializer function.
        // It receives `this` as receiver and defines each instance field.
        let class_name = class
            .id
            .as_ref()
            .map(|id| id.name.as_str())
            .unwrap_or("anonymous");
        let reserved = module.reserve_function();
        let mut init_compiler = FunctionCompiler::new(
            self.mode,
            Some(format!("{class_name}.__field_init__")),
            super::shared::FunctionKind::Ordinary,
            Some(self.env.clone()),
        );
        init_compiler.strict_mode = true;
        init_compiler.declare_parameters(&[])?;
        init_compiler.declare_this_binding()?;
        init_compiler.reserve_arguments_binding_slot()?;
        init_compiler.compile_parameter_initialization(&[], module)?;

        // Load `this` for field definitions.
        let this_reg = init_compiler.alloc_temp();
        init_compiler
            .instructions
            .push(Instruction::load_this(this_reg));
        let this_reg = init_compiler
            .stabilize_binding_value(ValueLocation::temp(this_reg))?
            .register;

        // Emit field definitions in source order.
        for element in &class.body.body {
            if let ClassElement::PropertyDefinition(prop) = element
                && !prop.r#static
                && !prop.declare
                && !matches!(
                    prop.r#type,
                    PropertyDefinitionType::TSAbstractPropertyDefinition
                )
            {
                if let oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) = &prop.key {
                    // §7.3.31 PrivateFieldAdd — private instance field.
                    let value = if let Some(init_expr) = &prop.value {
                        init_compiler.compile_expression(init_expr, module)?
                    } else {
                        init_compiler.load_undefined()?
                    };
                    let prop_id =
                        init_compiler.intern_property_name(ident.name.as_str())?;
                    init_compiler
                        .instructions
                        .push(Instruction::define_private_field(
                            this_reg,
                            value.register,
                            prop_id,
                        ));
                    init_compiler.release(value);
                } else {
                    // Public instance field.
                    let value = if let Some(init_expr) = &prop.value {
                        init_compiler.compile_expression(init_expr, module)?
                    } else {
                        init_compiler.load_undefined()?
                    };

                    if prop.computed {
                        let key =
                            init_compiler.compile_expression(prop.key.to_expression(), module)?;
                        init_compiler
                            .instructions
                            .push(Instruction::define_computed_field(
                                this_reg,
                                key.register,
                                value.register,
                            ));
                        init_compiler.release(key);
                    } else {
                        let key_name =
                            non_computed_property_key_name(&prop.key).ok_or_else(|| {
                                SourceLoweringError::Unsupported("unnamed class field".to_string())
                            })?;
                        let prop_id = init_compiler.intern_property_name(&key_name)?;
                        init_compiler.instructions.push(Instruction::define_field(
                            this_reg,
                            value.register,
                            prop_id,
                        ));
                    }
                    init_compiler.release(value);
                }
            }
        }

        init_compiler.emit_implicit_return()?;
        let compiled = init_compiler.finish(reserved, 0, Some("__field_init__"))?;
        module.set_function(reserved, compiled.function);

        // Create the closure and attach it to the constructor.
        let init_closure = ValueLocation::temp(self.alloc_temp());
        self.emit_new_closure(init_closure.register, reserved, &compiled.captures)?;

        // Propagate class_id so DefinePrivateField can resolve private names.
        if has_private_members {
            self.instructions.push(Instruction::copy_class_id(
                init_closure.register,
                constructor_value.register,
            ));
        }

        self.instructions
            .push(Instruction::set_class_field_initializer(
                constructor_value.register,
                init_closure.register,
            ));
        self.release(init_closure);

        Ok(())
    }

    /// §15.7.14 step 34 — Compile a single static field definition.
    /// Evaluates the initializer in a synthetic function with `this` = constructor,
    /// then defines the property on the constructor via DefineField.
    /// Spec: <https://tc39.es/ecma262/#sec-definefield>
    fn compile_static_field(
        &mut self,
        prop: &oxc_ast::ast::PropertyDefinition<'_>,
        constructor_value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::non_computed_property_key_name;

        if prop.computed {
            // Computed key: evaluate key in outer scope, value in a synthetic function.
            let key = self.compile_expression(prop.key.to_expression(), module)?;
            let key = self.stabilize_binding_value(key)?;

            let value = self.compile_static_field_value(prop, constructor_value, module)?;

            self.instructions.push(Instruction::define_computed_field(
                constructor_value.register,
                key.register,
                value.register,
            ));
            self.release(key);
            self.release(value);
        } else {
            let key_name = non_computed_property_key_name(&prop.key).ok_or_else(|| {
                SourceLoweringError::Unsupported("unnamed static field".to_string())
            })?;
            let prop_id = self.intern_property_name(&key_name)?;

            let value = self.compile_static_field_value(prop, constructor_value, module)?;

            self.instructions.push(Instruction::define_field(
                constructor_value.register,
                value.register,
                prop_id,
            ));
            self.release(value);
        }
        Ok(())
    }

    /// §15.7.14 — Compile a static private field definition.
    /// Evaluates the initializer via a synthetic function with `this` = constructor,
    /// then emits DefinePrivateField on the constructor.
    /// Spec: <https://tc39.es/ecma262/#sec-definefield>
    fn compile_static_private_field(
        &mut self,
        name: &str,
        prop: &oxc_ast::ast::PropertyDefinition<'_>,
        constructor_value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let value = self.compile_static_field_value(prop, constructor_value, module)?;
        let prop_id = self.intern_property_name(name)?;
        self.instructions.push(Instruction::define_private_field(
            constructor_value.register,
            value.register,
            prop_id,
        ));
        self.release(value);
        Ok(())
    }

    /// §15.7.14 — Compile a private class method/getter/setter.
    ///
    /// For **instance** methods: emits PushPrivateMethod/Getter/Setter on the
    /// constructor — these get copied to instances during RunClassFieldInitializer.
    /// For **static** methods: emits DefinePrivateMethod/Getter/Setter on the
    /// constructor directly (adds to constructor's [[PrivateElements]]).
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    fn compile_private_class_method(
        &mut self,
        method: &oxc_ast::ast::MethodDefinition<'_>,
        constructor_value: ValueLocation,
        prototype: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::extract_function_params;

        let private_name = match &method.key {
            oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) => ident.name.as_str(),
            _ => unreachable!("compile_private_class_method called with non-private key"),
        };

        let display_name = match method.kind {
            MethodDefinitionKind::Get => format!("get #{private_name}"),
            MethodDefinitionKind::Set => format!("set #{private_name}"),
            _ => format!("#{private_name}"),
        };

        // Compile the method body as a closure.
        let function = &method.value;
        let reserved = module.reserve_function();
        let params = extract_function_params(function)?;
        let kind = if method.value.generator && method.value.r#async {
            super::shared::FunctionKind::AsyncGenerator
        } else if method.value.generator {
            super::shared::FunctionKind::Generator
        } else if method.value.r#async {
            super::shared::FunctionKind::Async
        } else {
            super::shared::FunctionKind::Ordinary
        };
        let compiled = module.compile_function_from_statements(
            reserved,
            super::module_compiler::FunctionIdentity {
                debug_name: Some(display_name),
                self_binding_name: None,
                length: super::ast::expected_function_length(&params),
            },
            function
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .unwrap_or(&[]),
            &params,
            kind,
            Some(self.env.clone()),
            true, // class bodies are always strict
        )?;
        module.set_function(reserved, compiled.function);

        let method_closure = ValueLocation::temp(self.alloc_temp());
        if kind.is_generator() && kind.is_async() {
            self.emit_new_closure_async_generator(
                method_closure.register,
                reserved,
                &compiled.captures,
            )?;
        } else if kind.is_generator() {
            self.emit_new_closure_generator(
                method_closure.register,
                reserved,
                &compiled.captures,
            )?;
        } else if kind.is_async() {
            self.emit_new_closure_async(method_closure.register, reserved, &compiled.captures)?;
        } else {
            self.emit_new_closure(method_closure.register, reserved, &compiled.captures)?;
        }

        // Propagate class_id so the method can resolve private names.
        self.instructions.push(Instruction::copy_class_id(
            method_closure.register,
            constructor_value.register,
        ));

        let prop_id = self.intern_property_name(private_name)?;

        if method.r#static {
            // Static private: add directly to constructor's [[PrivateElements]].
            match method.kind {
                MethodDefinitionKind::Method => {
                    self.instructions.push(Instruction::define_private_method(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Get => {
                    self.instructions.push(Instruction::define_private_getter(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Set => {
                    self.instructions.push(Instruction::define_private_setter(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Constructor => {
                    unreachable!("constructor handled in first pass")
                }
            }
        } else {
            // Instance private: push to constructor's [[PrivateMethods]].
            // Copied to instances during RunClassFieldInitializer.
            match method.kind {
                MethodDefinitionKind::Method => {
                    self.instructions.push(Instruction::push_private_method(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Get => {
                    self.instructions.push(Instruction::push_private_getter(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Set => {
                    self.instructions.push(Instruction::push_private_setter(
                        constructor_value.register,
                        method_closure.register,
                        prop_id,
                    ));
                }
                MethodDefinitionKind::Constructor => {
                    unreachable!("constructor handled in first pass")
                }
            }
        }

        self.release(method_closure);
        let _ = prototype; // prototype is not used for private methods
        Ok(())
    }

    /// Compile a static field initializer value. If the field has an initializer,
    /// it's compiled as a synthetic function called with `this` = constructor
    /// (per spec, static field initializers evaluate with `this` bound to the class).
    /// Returns the result value location.
    fn compile_static_field_value(
        &mut self,
        prop: &oxc_ast::ast::PropertyDefinition<'_>,
        constructor_value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let Some(init_expr) = &prop.value else {
            return self.load_undefined();
        };

        // Compile the initializer as a synthetic function that returns the value.
        // This ensures `this` inside the initializer refers to the constructor.
        let reserved = module.reserve_function();
        let compiled = module.compile_function_from_expression(
            reserved,
            super::module_compiler::FunctionIdentity {
                debug_name: Some("static_field_init".to_string()),
                self_binding_name: None,
                length: 0,
            },
            init_expr,
            &[],
            super::shared::FunctionKind::Ordinary,
            Some(self.env.clone()),
            true,
        )?;
        module.set_function(reserved, compiled.function);

        let init_closure = ValueLocation::temp(self.alloc_temp());
        self.emit_new_closure(init_closure.register, reserved, &compiled.captures)?;

        // Call with constructor as receiver.
        let argument_count = 1u16;
        let arg_start = self.reserve_temp_window(argument_count)?;
        if constructor_value.register != arg_start {
            self.instructions
                .push(Instruction::move_(arg_start, constructor_value.register));
        }

        let result = self.alloc_temp();
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result,
            init_closure.register,
            arg_start,
        ));
        self.record_call_site(
            pc,
            crate::call::CallSite::Closure(crate::call::ClosureCall::new_with_receiver(
                argument_count,
                crate::frame::FrameFlags::new(false, true, false),
                arg_start,
            )),
        );
        self.release_temp_window(argument_count);
        self.release(init_closure);

        Ok(ValueLocation::temp(result))
    }

    /// §15.7.12 StaticBlock — `static { ... }`
    /// Compiled as an IIFE with `this` bound to the constructor.
    /// Spec: <https://tc39.es/ecma262/#sec-static-blocks>
    fn compile_static_block(
        &mut self,
        block: &oxc_ast::ast::StaticBlock<'_>,
        constructor_value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        // Compile the static block body as a synthetic function.
        let reserved = module.reserve_function();
        let compiled = module.compile_function_from_statements(
            reserved,
            super::module_compiler::FunctionIdentity {
                debug_name: Some("static".to_string()),
                self_binding_name: None,
                length: 0,
            },
            &block.body,
            &[],
            super::shared::FunctionKind::Ordinary,
            Some(self.env.clone()),
            true, // class bodies are always strict
        )?;
        module.set_function(reserved, compiled.function);

        // Create closure and immediately invoke with constructor as `this`.
        let block_closure = ValueLocation::temp(self.alloc_temp());
        self.emit_new_closure(block_closure.register, reserved, &compiled.captures)?;

        // Call: block_closure() with `this` = constructor.
        // argument_count = 1 because the receiver occupies one slot in the window.
        let argument_count = 1u16;
        let arg_start = self.reserve_temp_window(argument_count)?;
        if constructor_value.register != arg_start {
            self.instructions
                .push(Instruction::move_(arg_start, constructor_value.register));
        }

        let result = self.alloc_temp();
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result,
            block_closure.register,
            arg_start,
        ));
        self.record_call_site(
            pc,
            crate::call::CallSite::Closure(crate::call::ClosureCall::new_with_receiver(
                argument_count,
                crate::frame::FrameFlags::new(false, true, false),
                arg_start,
            )),
        );
        self.release(ValueLocation::temp(result));
        self.release_temp_window(argument_count);
        self.release(block_closure);
        Ok(())
    }

    /// Compiles a class method and installs it on the target (prototype or constructor).
    ///
    /// Handles regular methods, getters, setters — named and computed keys.
    /// If `class_id_source` is provided, emits CopyClassId on the closure before
    /// installing (so methods can resolve private names at runtime).
    /// §15.4.5 MethodDefinitionEvaluation
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-methoddefinitionevaluation>
    pub(super) fn compile_class_method(
        &mut self,
        method: &oxc_ast::ast::MethodDefinition<'_>,
        target: ValueLocation,
        class_id_source: Option<BytecodeRegister>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        use super::ast::{
            expected_function_length, extract_function_params, non_computed_property_key_name,
        };

        // Determine method name for debug/display.
        let method_name = if method.computed {
            None
        } else {
            non_computed_property_key_name(&method.key)
        };

        let display_name = match (&method.kind, &method_name) {
            (MethodDefinitionKind::Get, Some(n)) => Some(format!("get {n}")),
            (MethodDefinitionKind::Set, Some(n)) => Some(format!("set {n}")),
            (_, Some(n)) => Some(n.to_string()),
            _ => None,
        };

        // Compile the method body as a closure.
        let function = &method.value;
        let reserved = module.reserve_function();
        let params = extract_function_params(function)?;
        let kind = if method.value.generator && method.value.r#async {
            super::shared::FunctionKind::AsyncGenerator
        } else if method.value.generator {
            super::shared::FunctionKind::Generator
        } else if method.value.r#async {
            super::shared::FunctionKind::Async
        } else {
            super::shared::FunctionKind::Ordinary
        };
        let compiled = module.compile_function_from_statements(
            reserved,
            super::module_compiler::FunctionIdentity {
                debug_name: display_name.clone(),
                self_binding_name: None,
                length: expected_function_length(&params),
            },
            function
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .unwrap_or(&[]),
            &params,
            kind,
            Some(self.env.clone()),
            true, // class bodies are always strict
        )?;
        module.set_function(reserved, compiled.function);

        let method_closure = ValueLocation::temp(self.alloc_temp());
        if kind.is_generator() && kind.is_async() {
            self.emit_new_closure_async_generator(
                method_closure.register,
                reserved,
                &compiled.captures,
            )?;
        } else if kind.is_generator() {
            self.emit_new_closure_generator(
                method_closure.register,
                reserved,
                &compiled.captures,
            )?;
        } else if kind.is_async() {
            self.emit_new_closure_async(method_closure.register, reserved, &compiled.captures)?;
        } else {
            self.emit_new_closure(method_closure.register, reserved, &compiled.captures)?;
        }

        // Install on target: getter, setter, or data method.
        match method.kind {
            MethodDefinitionKind::Get => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions.push(Instruction::define_computed_getter(
                        target.register,
                        key.register,
                        method_closure.register,
                    ));
                    self.release(key);
                } else {
                    let name = method_name.as_ref().ok_or_else(|| {
                        SourceLoweringError::Unsupported("unnamed class getter".to_string())
                    })?;
                    let prop = self.intern_property_name(name)?;
                    self.instructions.push(Instruction::define_named_getter(
                        target.register,
                        method_closure.register,
                        prop,
                    ));
                }
            }
            MethodDefinitionKind::Set => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions.push(Instruction::define_computed_setter(
                        target.register,
                        key.register,
                        method_closure.register,
                    ));
                    self.release(key);
                } else {
                    let name = method_name.as_ref().ok_or_else(|| {
                        SourceLoweringError::Unsupported("unnamed class setter".to_string())
                    })?;
                    let prop = self.intern_property_name(name)?;
                    self.instructions.push(Instruction::define_named_setter(
                        target.register,
                        method_closure.register,
                        prop,
                    ));
                }
            }
            MethodDefinitionKind::Method => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions.push(Instruction::set_index(
                        target.register,
                        key.register,
                        method_closure.register,
                    ));
                    self.release(key);
                } else {
                    let name = method_name.as_ref().ok_or_else(|| {
                        SourceLoweringError::Unsupported("unnamed class method".to_string())
                    })?;
                    let prop = self.intern_property_name(name)?;
                    self.instructions.push(Instruction::set_property(
                        target.register,
                        method_closure.register,
                        prop,
                    ));
                }
            }
            MethodDefinitionKind::Constructor => unreachable!("constructor handled in first pass"),
        }

        // Propagate class_id so the method can resolve private names at runtime.
        if let Some(source) = class_id_source {
            self.instructions.push(Instruction::copy_class_id(
                method_closure.register,
                source,
            ));
        }

        self.release(method_closure);
        Ok(())
    }

    pub(super) fn compile_default_base_class_constructor(
        &mut self,
        class_name: &str,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let reserved = module.reserve_function();
        let compiled = module.compile_function_from_statements(
            reserved,
            FunctionIdentity {
                debug_name: Some(class_name.to_string()),
                self_binding_name: Some(class_name.to_string()),
                length: 0,
            },
            &[],
            &[],
            FunctionKind::Ordinary,
            Some(self.env.clone()),
            true,
        )?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_class_constructor(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    /// §15.7.14 — Compile an explicit class constructor with field support.
    /// For base class: emits RunClassFieldInitializer at the start.
    /// For derived class: relies on compile_super_call_* to emit RunClassFieldInitializer.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    fn compile_class_constructor_with_fields(
        &mut self,
        class_name: &str,
        constructor: &Function<'_>,
        derived: bool,
        has_instance_fields: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let reserved = module.reserve_function();
        let params = extract_function_params(constructor)?;

        // For base class constructors with fields, we need to inject
        // RunClassFieldInitializer at the start of the body. We do this by
        // adding has_instance_fields to the function compiler state.
        let mut compiler = FunctionCompiler::new(
            self.mode,
            Some(class_name.to_string()),
            FunctionKind::Ordinary,
            Some(self.env.clone()),
        );
        compiler.strict_mode = true;
        compiler.is_derived_constructor = derived;
        compiler.has_instance_fields = has_instance_fields;

        compiler.declare_parameters(&params)?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&params, module)?;
        if let Some(self_binding_name) = Some(class_name) {
            let closure_register = compiler.declare_function_binding(self_binding_name)?;
            compiler
                .instructions
                .push(Instruction::load_current_closure(closure_register));
        }

        // For base class: emit RunClassFieldInitializer before user code.
        if !derived && has_instance_fields {
            compiler
                .instructions
                .push(Instruction::run_class_field_initializer());
        }

        compiler.predeclare_function_scope(
            constructor
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .unwrap_or(&[]),
            module,
        )?;
        compiler.emit_hoisted_function_initializers()?;
        let terminated = compiler.compile_statements(
            constructor
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .unwrap_or(&[]),
            module,
        )?;
        if !terminated {
            compiler.emit_implicit_return()?;
        }

        let compiled = compiler.finish(
            reserved,
            expected_function_length(&params),
            Some(class_name),
        )?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_class_constructor(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    /// §15.7.14 — Default base class constructor with field initializer support.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    fn compile_default_base_class_constructor_with_fields(
        &mut self,
        class_name: &str,
        has_instance_fields: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if !has_instance_fields {
            return self.compile_default_base_class_constructor(class_name, module);
        }
        let reserved = module.reserve_function();
        let mut compiler = FunctionCompiler::new(
            self.mode,
            Some(class_name.to_string()),
            FunctionKind::Ordinary,
            Some(self.env.clone()),
        );
        compiler.strict_mode = true;
        compiler.has_instance_fields = true;
        compiler.declare_parameters(&[])?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&[], module)?;
        let closure_register = compiler.declare_function_binding(class_name)?;
        compiler
            .instructions
            .push(Instruction::load_current_closure(closure_register));

        // Emit RunClassFieldInitializer for instance fields.
        compiler
            .instructions
            .push(Instruction::run_class_field_initializer());

        compiler.emit_implicit_return()?;
        let compiled = compiler.finish(reserved, 0, Some(class_name))?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_class_constructor(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    /// §15.7.14 — Default derived class constructor with field initializer support.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    fn compile_default_derived_class_constructor_with_fields(
        &mut self,
        class_name: &str,
        has_instance_fields: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let reserved = module.reserve_function();
        let mut compiler = FunctionCompiler::new(
            self.mode,
            Some(class_name.to_string()),
            FunctionKind::Ordinary,
            Some(self.env.clone()),
        );
        compiler.strict_mode = true;
        compiler.is_derived_constructor = true;
        compiler.has_instance_fields = has_instance_fields;
        compiler.declare_parameters(&[])?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&[], module)?;
        let closure_register = compiler.declare_function_binding(class_name)?;
        compiler
            .instructions
            .push(Instruction::load_current_closure(closure_register));
        let forwarded = ValueLocation::temp(compiler.alloc_temp());
        compiler
            .instructions
            .push(Instruction::call_super_forward(forwarded.register));
        if let Some(Binding::ThisRegister(this_register)) =
            compiler.env.bindings.get("this").copied()
            && this_register != forwarded.register
        {
            compiler
                .instructions
                .push(Instruction::move_(this_register, forwarded.register));
        }

        // For derived class with fields: emit RunClassFieldInitializer after super().
        if has_instance_fields {
            compiler
                .instructions
                .push(Instruction::run_class_field_initializer());
        }

        compiler.release(forwarded);
        compiler.emit_implicit_return()?;
        let compiled = compiler.finish(reserved, 0, Some(class_name))?;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        self.emit_new_closure_class_constructor(destination, reserved, &compiled.captures)?;
        Ok(ValueLocation::temp(destination))
    }

    pub(super) fn emit_named_property_load(
        &mut self,
        base: ValueLocation,
        name: &str,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let property = self.intern_property_name(name)?;
        let result = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::get_property(
            result.register,
            base.register,
            property,
        ));
        Ok(result)
    }

    pub(super) fn emit_make_class_prototype_non_writable(
        &mut self,
        constructor: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let descriptor = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_object(descriptor.register));
        let writable = self.compile_bool(false)?;
        let writable_key = self.intern_property_name("writable")?;
        self.instructions.push(Instruction::set_property(
            descriptor.register,
            writable.register,
            writable_key,
        ));
        self.release(writable);

        let prototype_key = self.compile_string_literal("prototype")?;
        self.emit_object_method_call(
            "defineProperty",
            constructor,
            &[prototype_key, descriptor],
            module,
        )?;
        Ok(())
    }

    pub(super) fn emit_object_method_call(
        &mut self,
        method_name: &str,
        receiver: ValueLocation,
        args: &[ValueLocation],
        _module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let object = self.compile_identifier("Object")?;
        let object = if object.is_temp {
            self.stabilize_binding_value(object)?
        } else {
            object
        };
        let callee = self.emit_named_property_load(object, method_name)?;
        let callee = if callee.is_temp {
            self.stabilize_binding_value(callee)?
        } else {
            callee
        };

        let argument_count = RegisterIndex::try_from(args.len() + 1)
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let arg_start = self.reserve_temp_window(argument_count)?;
        let values: Vec<ValueLocation> = std::iter::once(receiver)
            .chain(args.iter().copied())
            .collect();
        for (offset, value) in values.into_iter().enumerate() {
            let destination = BytecodeRegister::new(arg_start.index() + offset as u16);
            if value.register != destination {
                self.instructions
                    .push(Instruction::move_(destination, value.register));
                self.release(value);
            }
        }

        let result = self.alloc_temp();
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            result,
            callee.register,
            arg_start,
        ));
        self.record_call_site(
            pc,
            CallSite::Closure(ClosureCall::new_with_receiver(
                argument_count,
                FrameFlags::new(false, true, false),
                object.register,
            )),
        );
        self.release(ValueLocation::temp(result));
        self.release_temp_window(argument_count);
        Ok(())
    }

    pub(super) fn compile_expression_statement(
        &mut self,
        expression: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        // Skip bare string literals (directive prologues like "use strict"),
        // except in eval mode where they contribute to the completion value.
        if matches!(expression, Expression::StringLiteral(_))
            && !(self.mode == LoweringMode::Eval && self.kind == FunctionKind::Script)
        {
            return Ok(());
        }

        let value = self.compile_expression(expression, module)?;

        // In eval mode at top-level, store every expression statement's result
        // as the completion value (ES spec: the value of the last evaluated
        // expression statement).
        if self.mode == LoweringMode::Eval && self.kind == FunctionKind::Script {
            let completion_reg = self.ensure_eval_completion_register()?;
            self.instructions
                .push(Instruction::move_(completion_reg, value.register));
        }

        self.release(value);
        Ok(())
    }

    /// Lazily allocates a local register for eval completion value tracking.
    fn ensure_eval_completion_register(&mut self) -> Result<BytecodeRegister, SourceLoweringError> {
        if let Some(reg) = self.eval_completion_register {
            return Ok(reg);
        }
        let reg = self.allocate_local()?;
        self.eval_completion_register = Some(reg);
        Ok(reg)
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

    pub(super) fn emit_new_closure_generator(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
    ) -> Result<(), SourceLoweringError> {
        self.emit_new_closure_with_flags(
            destination,
            callee,
            explicit_captures,
            crate::object::ClosureFlags::generator(),
        )
    }

    pub(super) fn emit_new_closure_async(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
    ) -> Result<(), SourceLoweringError> {
        self.emit_new_closure_with_flags(
            destination,
            callee,
            explicit_captures,
            crate::object::ClosureFlags::async_fn(),
        )
    }

    /// §27.6 Async generator closure — `async function*`.
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-objects>
    pub(super) fn emit_new_closure_async_generator(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
    ) -> Result<(), SourceLoweringError> {
        self.emit_new_closure_with_flags(
            destination,
            callee,
            explicit_captures,
            crate::object::ClosureFlags::async_generator(),
        )
    }

    pub(super) fn emit_new_closure_async_arrow(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
    ) -> Result<(), SourceLoweringError> {
        self.emit_new_closure_with_flags(
            destination,
            callee,
            explicit_captures,
            crate::object::ClosureFlags::async_arrow(),
        )
    }

    pub(super) fn emit_new_closure_class_constructor(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
    ) -> Result<(), SourceLoweringError> {
        self.emit_new_closure_with_flags(
            destination,
            callee,
            explicit_captures,
            crate::object::ClosureFlags::class_constructor(),
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
            Binding::ThisRegister(register) => {
                if register != value.register {
                    self.instructions
                        .push(Instruction::move_(register, value.register));
                    self.release(value);
                }
                Ok(ValueLocation::local(register))
            }
            Binding::ThisUpvalue(upvalue) => {
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
                Binding::ThisRegister(_) | Binding::Upvalue(_) | Binding::ThisUpvalue(_) => {
                    Err(SourceLoweringError::DuplicateBinding(name.to_string()))
                }
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
                Binding::ThisRegister(register) => register,
                Binding::Function {
                    closure_register, ..
                } => closure_register,
                Binding::Upvalue(_) | Binding::ThisUpvalue(_) => {
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

    pub(super) fn mirror_script_binding_to_global(
        &mut self,
        name: &str,
        register: BytecodeRegister,
    ) -> Result<(), SourceLoweringError> {
        if self.kind != FunctionKind::Script {
            return Ok(());
        }
        let property = self.intern_property_name(name)?;
        self.instructions
            .push(Instruction::set_global(register, property));
        Ok(())
    }

    pub(super) fn mirror_script_binding_to_global_by_register(
        &mut self,
        register: BytecodeRegister,
    ) -> Result<(), SourceLoweringError> {
        if self.kind != FunctionKind::Script {
            return Ok(());
        }
        let Some(name) = self
            .env
            .bindings
            .iter()
            .find(|(_, binding)| {
                matches!(
                    binding,
                    Binding::Function { closure_register } if *closure_register == register
                )
            })
            .map(|(name, _)| name.clone())
        else {
            return Ok(());
        };
        let property = self.intern_property_name(&name)?;
        self.instructions
            .push(Instruction::set_global(register, property));
        Ok(())
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
            let captured = match binding {
                Binding::ThisRegister(_) | Binding::ThisUpvalue(_) => Binding::ThisUpvalue(upvalue),
                _ => Binding::Upvalue(upvalue),
            };
            self.env.bindings.insert(name.to_string(), captured);
            return Ok(captured);
        }

        // ES2024 §10.4.4: `arguments` is implicitly available in non-arrow functions.
        // Lazily allocate a local and emit CreateArguments on first access.
        if name == "arguments" && self.kind != FunctionKind::Arrow {
            let register = if let Some(reg) = self.arguments_local {
                reg
            } else {
                let reg = self.allocate_local()?;
                self.arguments_local = Some(reg);
                reg
            };
            if !self.env.bindings.contains_key("arguments") {
                self.instructions
                    .push(Instruction::create_arguments(register));
                self.env
                    .bindings
                    .insert("arguments".to_string(), Binding::Register(register));
            }
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
                LoweringMode::Eval => {
                    if let Some(reg) = self.eval_completion_register {
                        // Return the completion value collected during execution.
                        self.instructions.push(Instruction::ret(reg));
                        return Ok(());
                    }
                    // No expression statements were compiled; return undefined.
                    self.load_undefined()?
                }
            },
            FunctionKind::Ordinary
            | FunctionKind::Arrow
            | FunctionKind::Generator
            | FunctionKind::Async
            | FunctionKind::AsyncArrow
            | FunctionKind::AsyncGenerator => self.load_undefined()?,
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

    /// Interns a BigInt constant value and returns its stable id.
    ///
    /// §6.1.6.2 The BigInt Type
    /// Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    pub(super) fn intern_bigint(
        &mut self,
        value: &str,
    ) -> Result<crate::bigint::BigIntId, SourceLoweringError> {
        if let Some(existing) = self.bigint_ids.get(value).copied() {
            return Ok(existing);
        }
        let id = crate::bigint::BigIntId(
            u16::try_from(self.bigint_constants.len())
                .map_err(|_| SourceLoweringError::TooManyLocals)?,
        );
        self.bigint_constants
            .push(value.to_string().into_boxed_str());
        self.bigint_ids.insert(value.to_string(), id);
        Ok(id)
    }

    /// Interns a RegExp literal `(pattern, flags)` pair and returns its stable id.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-literals-regular-expression-literals>
    pub(super) fn intern_regexp(
        &mut self,
        pattern: &str,
        flags: &str,
    ) -> Result<crate::regexp::RegExpId, SourceLoweringError> {
        let key = (pattern.to_string(), flags.to_string());
        if let Some(existing) = self.regexp_ids.get(&key).copied() {
            return Ok(existing);
        }
        let id = crate::regexp::RegExpId(
            u16::try_from(self.regexp_literals.len())
                .map_err(|_| SourceLoweringError::TooManyLocals)?,
        );
        self.regexp_literals.push((
            pattern.to_string().into_boxed_str(),
            flags.to_string().into_boxed_str(),
        ));
        self.regexp_ids.insert(key, id);
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
