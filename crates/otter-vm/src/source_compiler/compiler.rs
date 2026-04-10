use super::ast::{
    ParamInfo, collect_binding_identifier_names, collect_function_declarations, collect_var_names,
    expected_function_length, extract_function_params, identifier_name_for_parameter_pattern,
    is_test262_failure_throw,
};
use super::module_compiler::{FunctionIdentity, ModuleCompiler};
use super::shared::{
    Binding, CaptureSource, CompiledFunction, FunctionCompiler, FunctionKind, LoopScope,
    PendingFunction, ScopeRef, ValueLocation, new_scope_ref,
};
use super::source_mapper::SourceMapper;
use super::*;
use crate::source_map::SourceMapEntry;
use oxc_span::{GetSpan, Span};
use std::rc::Rc;

impl<'a> FunctionCompiler<'a> {
    pub(super) fn new(
        mode: LoweringMode,
        function_name: Option<String>,
        kind: FunctionKind,
        parent_scopes: Vec<ScopeRef>,
        source_mapper: Rc<SourceMapper>,
    ) -> Self {
        Self {
            mode,
            strict_mode: false,
            is_derived_constructor: false,
            has_instance_fields: false,
            function_name,
            kind,
            parent_scopes,
            scope: new_scope_ref(),
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
            hoisted_functions: Vec::new(),
            finally_stack: Vec::new(),
            loop_stack: Vec::new(),
            pending_loop_label: None,
            arguments_local: None,
            rest_local: None,
            parameter_binding_registers: Vec::new(),
            parameter_tdz_active: false,
            predeclared_lexical_names: std::collections::BTreeSet::new(),
            eval_completion_register: None,
            source_mapper,
            source_map_entries: Vec::new(),
            last_recorded_location: None,
            pending_site_span: None,
            private_name_scopes: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Records a source-map entry at the current program counter for the
    /// given AST span. Coalesces consecutive identical locations so the
    /// resulting `SourceMap` only contains distinct entries.
    ///
    /// Callers pass the span of whatever AST node they are about to compile
    /// (statement, expression, or even a single `throw`). The first entry
    /// wins on ties so the span of the outer node defines the attribution
    /// for the instructions that follow until the next `record_location`.
    pub(super) fn record_location(&mut self, span: Span) {
        if span.is_unspanned() {
            return;
        }
        let location = self.source_mapper.locate(span);
        if self.last_recorded_location == Some(location) {
            return;
        }
        let pc = u32::try_from(self.instructions.len()).unwrap_or(u32::MAX);
        // If the previous recorded entry was at the same pc, update it in
        // place so only the most recent (typically the more specific) span
        // wins. This happens when `record_location` is called multiple times
        // before any instruction is emitted (e.g., at the start of a block).
        if let Some(last) = self.source_map_entries.last_mut()
            && last.pc() == pc
        {
            *last = SourceMapEntry::new(pc, location);
            self.last_recorded_location = Some(location);
            return;
        }
        self.source_map_entries
            .push(SourceMapEntry::new(pc, location));
        self.last_recorded_location = Some(location);
    }

    /// Returns the parent scope chain a child function compiler should
    /// inherit: this function's own scope first, then this function's
    /// existing parent scopes. Each is an `Rc` clone — child compilations
    /// share the same `RefCell`s and can materialize upvalues into
    /// intermediate ancestors via `borrow_mut`.
    pub(super) fn parent_scopes_for_child(&self) -> Vec<ScopeRef> {
        let mut chain = Vec::with_capacity(self.parent_scopes.len() + 1);
        chain.push(self.scope.clone());
        for parent in &self.parent_scopes {
            chain.push(parent.clone());
        }
        chain
    }

    fn declare_parameter_pattern_bindings(
        &mut self,
        pattern: &BindingPattern<'_>,
    ) -> Result<(), SourceLoweringError> {
        let mut names = Vec::new();
        collect_binding_identifier_names(pattern, &mut names);
        for name in names {
            let register = self.allocate_local()?;
            self.scope
                .borrow_mut()
                .bindings
                .insert(name, Binding::Register(register));
            self.parameter_binding_registers.push(register);
        }
        Ok(())
    }

    pub(super) fn declare_parameters(
        &mut self,
        params: &[ParamInfo<'_>],
    ) -> Result<(), SourceLoweringError> {
        // §15.1 — Strict mode functions must not have duplicate parameter names.
        // Spec: <https://tc39.es/ecma262/#sec-function-definitions-static-semantics-early-errors>
        let mut seen_names: Option<std::collections::HashSet<String>> = if self.strict_mode {
            Some(std::collections::HashSet::new())
        } else {
            None
        };

        // §15.1 — Additional early errors: in strict mode it is a SyntaxError
        // if any BoundNames element is "eval" or "arguments". Applies to every
        // formal parameter binding, including those inside destructuring and
        // rest patterns. (oxc's parser does not flag this on its own.)
        let reject_strict_reserved_name =
            |name: &str, strict: bool| -> Result<(), SourceLoweringError> {
                if strict && (name == "eval" || name == "arguments") {
                    return Err(SourceLoweringError::EarlyError(format!(
                        "`{name}` cannot be used as a formal parameter name in strict mode"
                    )));
                }
                Ok(())
            };

        for param in params {
            if param.is_rest {
                let register = self.allocate_local()?;
                if let Some(name) = identifier_name_for_parameter_pattern(param.pattern) {
                    reject_strict_reserved_name(name, self.strict_mode)?;
                    if let Some(ref mut seen) = seen_names
                        && !seen.insert(name.to_string())
                    {
                        return Err(SourceLoweringError::DuplicateBinding(name.to_string()));
                    }
                    self.scope
                        .borrow_mut()
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
                reject_strict_reserved_name(name, self.strict_mode)?;
                if let Some(ref mut seen) = seen_names
                    && !seen.insert(name.to_string())
                {
                    return Err(SourceLoweringError::DuplicateBinding(name.to_string()));
                }
                self.scope
                    .borrow_mut()
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
        self.scope
            .borrow_mut()
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

        // §9.1.2 GetIdentifierReference + §10.2.11 FunctionDeclarationInstantiation:
        // Pre-declare top-level `let`/`const`/`class` bindings BEFORE compiling
        // hoisted nested function declarations. Otherwise the nested closures
        // can't see top-level lexical names via the scope chain at compile
        // time and will fall back to a runtime global lookup that misses the
        // (non-global) script-level lexical environment.
        //
        // The bindings are initially in TDZ — `load_hole` so any read before
        // the actual `let foo = ...` statement throws ReferenceError. The
        // real declaration claims the same register slot via
        // `declare_variable_binding`.
        for name in super::ast::collect_top_level_lexical_names(statements) {
            if self.scope.borrow().bindings.contains_key(&name) {
                continue;
            }
            let register = self.allocate_local()?;
            self.instructions.push(Instruction::load_hole(register));
            self.scope
                .borrow_mut()
                .bindings
                .insert(name.clone(), Binding::Register(register));
            self.predeclared_lexical_names.insert(name);
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
                self.parent_scopes_for_child(),
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
                StringTable::new_js(self.string_literals),
                FloatTable::new(self.float_constants),
                crate::bigint::BigIntTable::new(self.bigint_constants),
                ClosureTable::new(self.closure_templates),
                CallTable::new(self.call_sites),
                crate::regexp::RegExpTable::new(self.regexp_literals),
            ),
            FeedbackTableLayout::default(),
            DeoptTable::default(),
            ExceptionTable::new(self.exception_handlers),
            SourceMap::new(self.source_map_entries),
        );

        // The child compiler owns its scope frame uniquely (the parent only
        // holds clones of *ancestor* scopes, never of this child's own
        // scope), so unwrapping the `Rc` here always succeeds. We extract
        // the captures vec without an extra clone.
        let captures = std::rc::Rc::try_unwrap(self.scope)
            .map(|cell| cell.into_inner().captures)
            .unwrap_or_else(|shared| shared.borrow().captures.clone());

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
            captures,
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
        // Tag the next bytecode instructions with this statement's span so
        // runtime stack traces and diagnostics point at the right line/col.
        // `record_location` dedups identical `(line, col)` so this is cheap
        // on tight expression sequences.
        self.record_location(statement.span());
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
                // Re-record the throw statement's own span just before
                // emitting the throw opcode so the diagnostic underline
                // lands on the `throw` keyword, not on the last
                // sub-expression that happened to set the active span.
                self.record_location(throw_statement.span);
                self.instructions.push(Instruction::throw(value.register));
                self.release(value);
                Ok(true)
            }
            AstStatement::SwitchStatement(switch) => self.compile_switch_statement(switch, module),

            // ═══════════════════════════════════════════════════════════════
            //  §16.2 — Module Declarations (import/export)
            //  Spec: <https://tc39.es/ecma262/#sec-modules>
            // ═══════════════════════════════════════════════════════════════
            AstStatement::ImportDeclaration(import) => {
                self.compile_import_declaration(import, module)?;
                Ok(false)
            }
            AstStatement::ExportNamedDeclaration(export) => {
                self.compile_export_named_declaration(export, module)?;
                Ok(false)
            }
            AstStatement::ExportDefaultDeclaration(export) => {
                self.compile_export_default_declaration(export, module)?;
                Ok(false)
            }
            AstStatement::ExportAllDeclaration(export) => {
                self.compile_export_all_declaration(export, module)?;
                Ok(false)
            }

            // §14.15 — debugger statement: no-op in production
            AstStatement::DebuggerStatement(_) => Ok(false),

            // §14.11 — with statement: forbidden in strict mode
            AstStatement::WithStatement(_with) => {
                // oxc parser already rejects `with` in strict mode. In sloppy mode
                // we compile the body ignoring the with-object (partial semantics —
                // full scope chain injection not yet implemented).
                self.compile_statement(&_with.body, module)
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
    pub(super) fn compile_class_declaration(
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

    /// §15.7.14 PrivateBoundNames uniqueness check.
    ///
    /// Collects every private declaration in the class body (private fields,
    /// private methods, private getters, private setters — instance and
    /// static alike) and rejects duplicate entries per the spec's early-error
    /// rule. The only allowed duplication is a `{getter, setter}` pair that
    /// shares its `static` flag with no other entries on the same name.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-static-semantics-privateboundnames>
    /// Spec: <https://tc39.es/ecma262/#sec-class-definitions-static-semantics-early-errors>
    fn validate_private_bound_names(
        &self,
        class: &Class<'_>,
    ) -> Result<std::collections::HashSet<String>, SourceLoweringError> {
        use oxc_ast::ast::{ClassElement, MethodDefinitionKind, PropertyDefinitionType};

        /// One private declaration kind plus its static-ness.
        #[derive(Debug, Clone, Copy)]
        enum PrivateEntry {
            Field,
            Method,
            Getter { is_static: bool },
            Setter { is_static: bool },
        }

        // PrivateBoundNames treats static and instance members as the same
        // namespace (§8.2.3), so we key by the bare name and collect the
        // list of entries observed for each.
        let mut seen: std::collections::HashMap<String, Vec<PrivateEntry>> =
            std::collections::HashMap::new();

        let mut record = |name: &str,
                          entry: PrivateEntry|
         -> Result<(), SourceLoweringError> {
            let list = seen.entry(name.to_string()).or_default();
            list.push(entry);
            // Allowed multiplicities:
            //   - 1 entry of any kind
            //   - 2 entries iff they are a getter+setter pair that share the
            //     same static-ness (everything else is a duplicate).
            let ok = match list.as_slice() {
                [] => unreachable!(),
                [_] => true,
                [a, b] => matches!(
                    (*a, *b),
                    (
                        PrivateEntry::Getter { is_static: s1 },
                        PrivateEntry::Setter { is_static: s2 },
                    ) | (
                        PrivateEntry::Setter { is_static: s1 },
                        PrivateEntry::Getter { is_static: s2 },
                    ) if s1 == s2
                ),
                _ => false,
            };
            if !ok {
                return Err(SourceLoweringError::EarlyError(format!(
                    "Duplicate private name declaration: #{name}"
                )));
            }
            Ok(())
        };

        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(method) => {
                    let oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) = &method.key else {
                        continue;
                    };
                    let name = ident.name.as_str();
                    let is_static = method.r#static;
                    let entry = match method.kind {
                        MethodDefinitionKind::Method => PrivateEntry::Method,
                        MethodDefinitionKind::Get => PrivateEntry::Getter { is_static },
                        MethodDefinitionKind::Set => PrivateEntry::Setter { is_static },
                        MethodDefinitionKind::Constructor => continue,
                    };
                    record(name, entry)?;
                }
                ClassElement::PropertyDefinition(prop) => {
                    if prop.declare
                        || matches!(
                            prop.r#type,
                            PropertyDefinitionType::TSAbstractPropertyDefinition
                        )
                    {
                        continue;
                    }
                    let oxc_ast::ast::PropertyKey::PrivateIdentifier(ident) = &prop.key else {
                        continue;
                    };
                    record(ident.name.as_str(), PrivateEntry::Field)?;
                }
                _ => {}
            }
        }

        Ok(seen.keys().cloned().collect())
    }

    /// §15.7.14 / §8.3 AllPrivateNamesValid — walk the class body and verify
    /// that every `#name` reference inside it resolves to a declaration in
    /// either the current class or an enclosing class's private environment.
    /// Nested classes are skipped (they validate on their own).
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-static-semantics-allprivatenamesvalid>
    fn validate_private_name_references(
        &self,
        class: &Class<'_>,
        declared_here: &std::collections::HashSet<String>,
    ) -> Result<(), SourceLoweringError> {
        use oxc_ast::ast::ClassElement;
        use super::ast::{check_expression_private_refs, check_statement_private_refs};

        let is_declared = |name: &str| -> bool {
            if declared_here.contains(name) {
                return true;
            }
            self.private_name_scopes
                .iter()
                .any(|scope| scope.contains(name))
        };

        // Heritage expression (`extends Foo`) is evaluated *before* the
        // class body's private environment is established, so private
        // references inside it may only resolve to outer scopes — never
        // the current class's declarations.
        if let Some(super_class) = class.super_class.as_ref() {
            let is_declared_outer = |name: &str| -> bool {
                self.private_name_scopes
                    .iter()
                    .any(|scope| scope.contains(name))
            };
            check_expression_private_refs(super_class, &is_declared_outer)?;
        }

        for element in &class.body.body {
            match element {
                ClassElement::PropertyDefinition(prop) => {
                    if prop.declare {
                        continue;
                    }
                    if prop.computed
                        && let Some(key_expr) = prop.key.as_expression()
                    {
                        check_expression_private_refs(key_expr, &is_declared)?;
                    }
                    if let Some(value) = prop.value.as_ref() {
                        check_expression_private_refs(value, &is_declared)?;
                    }
                }
                ClassElement::MethodDefinition(method) => {
                    if method.computed
                        && let Some(key_expr) = method.key.as_expression()
                    {
                        check_expression_private_refs(key_expr, &is_declared)?;
                    }
                    if let Some(body) = method.value.body.as_ref() {
                        for stmt in &body.statements {
                            check_statement_private_refs(stmt, &is_declared)?;
                        }
                    }
                }
                ClassElement::StaticBlock(block) => {
                    for stmt in &block.body {
                        check_statement_private_refs(stmt, &is_declared)?;
                    }
                }
                _ => {}
            }
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
        // §15.7.14 Static Semantics: Early Errors — PrivateBoundNames of
        // ClassBody must not contain duplicates, unless a name is used
        // exactly once for a getter and once for a setter with matching
        // static-ness (and no other entries).
        // Spec: <https://tc39.es/ecma262/#sec-static-semantics-privateboundnames>
        let declared_private_names = self.validate_private_bound_names(class)?;

        // §15.7.14 / §8.3 AllPrivateNamesValid — verify every `#name`
        // reference inside the class body resolves against the current class
        // or an enclosing lexical class's private environment.
        self.validate_private_name_references(class, &declared_private_names)?;

        // Push this class's declared private names so nested classes and
        // closures compiled as part of this body see them when validating
        // their own private references. Mirror the change onto the module
        // compiler's pending latch so freshly constructed child
        // `FunctionCompiler`s pick up the inherited chain too.
        self.private_name_scopes.push(declared_private_names.clone());
        module
            .pending_private_name_scopes
            .push(declared_private_names);
        let result = self.compile_class_body_inner(class, class_name, module);
        self.private_name_scopes.pop();
        module.pending_private_name_scopes.pop();
        result
    }

    /// Inner implementation of `compile_class_body` that runs after the
    /// `PrivateNameEnvironment` has been pushed for this class.
    fn compile_class_body_inner(
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
                    if matches!(&method.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_)) {
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
                    if matches!(&prop.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_)) {
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

        // §15.7.15 ClassDefinitionEvaluation step 2-3: Create a lexical
        // binding for the class name inside the class body scope. Named
        // classes expose an immutable inner binding so `class C { m() { C } }`
        // resolves `C` inside methods. The binding starts in TDZ (hole) so
        // `class x extends x {}` throws ReferenceError from the extends
        // expression, and is initialized after the constructor is compiled.
        // Anonymous class expressions (via NamedEvaluation) do NOT get this
        // binding — references to the contextual name resolve to the outer
        // variable instead (§15.7.15).
        let class_has_self_binding = class.id.is_some();
        // §15.7.15 step 2-3: Named classes get a TDZ inner binding. We
        // allocate the local in the outer FC but do NOT insert the binding
        // into `self.scope` — instead we'll push a temporary class-scope
        // frame that only child compilations (methods, field initialisers)
        // see via `parent_scopes_for_child()`. This avoids leaking the
        // immutable binding into the outer scope where `var C = class C {}`
        // would collide with it.
        let class_name_register = if class_has_self_binding && !class_name.is_empty() {
            let register = self.allocate_local()?;
            self.instructions.push(Instruction::load_hole(register));
            Some(register)
        } else {
            None
        };

        // ── Compile super class ─────────────────────────────────────────────
        // §15.7.14 step 5: Detect `class extends null` — protoParent = null,
        // constructorParent = %Function.prototype%, constructor kind = base.
        // Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
        let extends_null = matches!(class.super_class.as_ref(), Some(Expression::NullLiteral(_)));
        // §15.7.14 step 7: Evaluate ClassHeritage in the *outer*
        // PrivateEnvironment. Temporarily pop this class's declared-names
        // frame (pushed by `compile_class_body`) so that any nested class
        // expressions inside the heritage expression see only the enclosing
        // lexical classes — not the class currently being defined.
        let super_class = if let Some(super_class) = class.super_class.as_ref() {
            if extends_null {
                None // Don't compile null as a super class value
            } else {
                let saved_fc = self.private_name_scopes.pop();
                let saved_mc = module.pending_private_name_scopes.pop();
                let super_result = self.compile_expression(super_class, module);
                if let Some(frame) = saved_fc {
                    self.private_name_scopes.push(frame);
                }
                if let Some(frame) = saved_mc {
                    module.pending_private_name_scopes.push(frame);
                }
                let super_value = super_result?;
                Some(self.stabilize_binding_value(super_value)?)
            }
        } else {
            None
        };
        // §15.7.14 — A class with `extends` (including `extends null`) is
        // derived. `extends null` means the constructor CAN contain `super()`
        // but the call will throw TypeError at runtime because null is not
        // a constructor.
        let is_derived = class.super_class.is_some();

        // §15.7.14 step 5.f: if superclass is not null and not a
        // constructor, throw TypeError BEFORE reading `.prototype`.
        if let Some(ref sc) = super_class {
            self.instructions
                .push(Instruction::assert_constructor(sc.register));
        }

        // ── Compile constructor ─────────────────────────────────────────────
        // RunClassFieldInitializer is needed both for instance fields AND for
        // copying private methods/accessors to instances.
        let needs_field_initializer = has_instance_fields || has_private_members;
        // §15.7.15 — Push the class name inner binding into scope BEFORE
        // constructor compilation so the ctor body sees the immutable
        // binding via parent scope chain. The register holds hole at this
        // point; it will be initialised with the constructor value below.
        // When the immutable binding is already in scope, the constructor
        // must NOT create its own `declare_function_binding` — it captures
        // the class name via upvalue from the outer ImmutableRegister.
        let saved_class_name_binding = if let Some(class_reg) = class_name_register
            && !class_name.is_empty()
        {
            let old = self
                .scope
                .borrow_mut()
                .bindings
                .insert(class_name.to_string(), Binding::ImmutableRegister(class_reg));
            Some(old)
        } else {
            None
        };
        // When the outer scope already has the class name as
        // ImmutableRegister, the constructor must NOT create its own
        // declare_function_binding — it should capture via upvalue
        // (ImmutableUpvalue) instead, so writes trigger ThrowConstAssign.
        let ctor_has_self_binding = if saved_class_name_binding.is_some() {
            false
        } else {
            class_has_self_binding
        };
        let constructor_value = if let Some(ctor) = constructor {
            self.compile_class_constructor_with_fields(
                class_name,
                ctor_has_self_binding,
                ctor,
                is_derived,
                needs_field_initializer,
                module,
            )?
        } else if is_derived {
            self.compile_default_derived_class_constructor_with_fields(
                class_name,
                ctor_has_self_binding,
                needs_field_initializer,
                module,
            )?
        } else {
            self.compile_default_base_class_constructor_with_fields(
                class_name,
                ctor_has_self_binding,
                needs_field_initializer,
                module,
            )?
        };
        let constructor_value = if constructor_value.is_temp {
            self.stabilize_binding_value(constructor_value)?
        } else {
            constructor_value
        };

        // §15.7.15 step 12: Initialize the class name binding.
        // Now that the constructor closure exists, move its value into the
        // TDZ register allocated earlier so the body scope sees the live
        // constructor instead of the initial hole sentinel.
        if let Some(class_reg) = class_name_register {
            if class_reg != constructor_value.register {
                self.instructions
                    .push(Instruction::move_(class_reg, constructor_value.register));
            }
        }

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

        // §10.2.5 MakeMethod — set `[[HomeObject]]` on the class constructor
        // so that `super.foo` inside the constructor body resolves against
        // `prototype.[[Prototype]]` (which is `SuperClass.prototype`).
        // Static methods and the constructor itself share the constructor
        // as their HomeObject: per spec the static part of a derived class
        // has `home_object = constructor`, so `super.foo` in a static method
        // walks from `constructor.[[Prototype]]` (the parent class
        // constructor). The instance constructor uses `prototype` as its
        // HomeObject so instance `super.foo` resolves against
        // `prototype.[[Prototype]]`.
        self.instructions.push(Instruction::set_home_object(
            constructor_value.register,
            prototype.register,
        ));

        // ── Second pass: install methods ────────────────────────────────────
        // §15.7.14 ClassDefinitionEvaluation step 26–28.
        for element in &class.body.body {
            if let ClassElement::MethodDefinition(method) = element
                && !matches!(method.kind, MethodDefinitionKind::Constructor)
            {
                let is_private =
                    matches!(&method.key, oxc_ast::ast::PropertyKey::PrivateIdentifier(_));
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

        // §15.7.15 — Restore the previous class name binding (or remove
        // the temporary one if there was no prior binding).
        if let Some(old_binding) = saved_class_name_binding {
            match old_binding {
                Some(prev) => {
                    self.scope
                        .borrow_mut()
                        .bindings
                        .insert(class_name.to_string(), prev);
                }
                None => {
                    self.scope.borrow_mut().bindings.remove(class_name);
                }
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
            self.parent_scopes_for_child(),
            module.source_mapper(),
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
                    let prop_id = init_compiler.intern_property_name(ident.name.as_str())?;
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
            self.parent_scopes_for_child(),
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
            self.emit_new_closure_generator(method_closure.register, reserved, &compiled.captures)?;
        } else if kind.is_async() {
            self.emit_new_closure_async(method_closure.register, reserved, &compiled.captures)?;
        } else {
            // §15.4.4 — private methods are MethodDefinitions; no own .prototype.
            self.emit_new_closure_method(method_closure.register, reserved, &compiled.captures)?;
        }

        // Propagate class_id so the method can resolve private names.
        self.instructions.push(Instruction::copy_class_id(
            method_closure.register,
            constructor_value.register,
        ));

        // §10.2.5 MakeMethod — set `[[HomeObject]]` so `super.foo` inside the
        // private method body resolves correctly. Static private methods
        // use the constructor; instance private methods use the prototype.
        let home_register = if method.r#static {
            constructor_value.register
        } else {
            prototype.register
        };
        self.instructions.push(Instruction::set_home_object(
            method_closure.register,
            home_register,
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
            self.parent_scopes_for_child(),
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
            self.parent_scopes_for_child(),
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
        // §15.2.1.1 / §15.4.1 MethodDefinition Static Semantics: Early Errors —
        // It is a SyntaxError if `FunctionBody` Contains `"use strict"` and
        // `IsSimpleParameterList(FormalParameters)` is false.
        if let Some(body) = function.body.as_ref()
            && super::ast::has_use_strict_directive(&body.directives)
            && !super::ast::is_simple_parameter_list(&params)
        {
            return Err(SourceLoweringError::EarlyError(format!(
                "Illegal 'use strict' directive in function `{}` with non-simple parameter list",
                display_name.as_deref().unwrap_or("<anonymous>")
            )));
        }
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
            self.parent_scopes_for_child(),
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
            self.emit_new_closure_generator(method_closure.register, reserved, &compiled.captures)?;
        } else if kind.is_async() {
            self.emit_new_closure_async(method_closure.register, reserved, &compiled.captures)?;
        } else {
            // §15.4.4 — class methods, getters, and setters are
            // MethodDefinitions and must not be constructors (§10.2 MakeMethod).
            // Use the method closure flag so no own `.prototype` is installed.
            self.emit_new_closure_method(method_closure.register, reserved, &compiled.captures)?;
        }

        // §10.2.5 MakeMethod — set `[[HomeObject]]` on the method closure so
        // that subsequent `super.foo` / `super[x]` references inside the body
        // resolve against `HomeObject.[[Prototype]]`. `target` is the
        // prototype for instance members or the constructor for static
        // members, which is exactly what the spec wants.
        self.instructions.push(Instruction::set_home_object(
            method_closure.register,
            target.register,
        ));

        // Install on target: getter, setter, or data method.
        // §15.7.14 ClassDefinitionEvaluation step 28 — class methods have
        // `[[Enumerable]]: false` and are installed via `[[DefineOwnProperty]]`.
        match method.kind {
            MethodDefinitionKind::Get => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions
                        .push(Instruction::define_class_getter_computed(
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
                    self.instructions.push(Instruction::define_class_getter(
                        target.register,
                        method_closure.register,
                        prop,
                    ));
                }
            }
            MethodDefinitionKind::Set => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions
                        .push(Instruction::define_class_setter_computed(
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
                    self.instructions.push(Instruction::define_class_setter(
                        target.register,
                        method_closure.register,
                        prop,
                    ));
                }
            }
            MethodDefinitionKind::Method => {
                if method.computed {
                    let key = self.compile_expression(method.key.to_expression(), module)?;
                    self.instructions
                        .push(Instruction::define_class_method_computed(
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
                    self.instructions.push(Instruction::define_class_method(
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
            self.instructions
                .push(Instruction::copy_class_id(method_closure.register, source));
        }

        self.release(method_closure);
        Ok(())
    }

    pub(super) fn compile_default_base_class_constructor(
        &mut self,
        class_name: &str,
        has_self_binding: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let reserved = module.reserve_function();
        let compiled = module.compile_function_from_statements(
            reserved,
            FunctionIdentity {
                debug_name: Some(class_name.to_string()),
                self_binding_name: if has_self_binding {
                    Some(class_name.to_string())
                } else {
                    None
                },
                length: 0,
            },
            &[],
            &[],
            FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
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
        has_self_binding: bool,
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
            self.parent_scopes_for_child(),
            module.source_mapper(),
        );
        compiler.strict_mode = true;
        compiler.is_derived_constructor = derived;
        compiler.has_instance_fields = has_instance_fields;

        compiler.declare_parameters(&params)?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&params, module)?;
        // §15.7.15 step 12.b: only named classes get an inner self-binding
        // for the class body.
        if has_self_binding {
            let closure_register = compiler.declare_function_binding(class_name)?;
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
        has_self_binding: bool,
        has_instance_fields: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if !has_instance_fields {
            return self.compile_default_base_class_constructor(
                class_name,
                has_self_binding,
                module,
            );
        }
        let reserved = module.reserve_function();
        let mut compiler = FunctionCompiler::new(
            self.mode,
            Some(class_name.to_string()),
            FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            module.source_mapper(),
        );
        compiler.strict_mode = true;
        compiler.has_instance_fields = true;
        compiler.declare_parameters(&[])?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&[], module)?;
        if has_self_binding {
            let closure_register = compiler.declare_function_binding(class_name)?;
            compiler
                .instructions
                .push(Instruction::load_current_closure(closure_register));
        }

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
        has_self_binding: bool,
        has_instance_fields: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let reserved = module.reserve_function();
        let mut compiler = FunctionCompiler::new(
            self.mode,
            Some(class_name.to_string()),
            FunctionKind::Ordinary,
            self.parent_scopes_for_child(),
            module.source_mapper(),
        );
        compiler.strict_mode = true;
        compiler.is_derived_constructor = true;
        compiler.has_instance_fields = has_instance_fields;
        compiler.declare_parameters(&[])?;
        compiler.declare_this_binding()?;
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(&[], module)?;
        if has_self_binding {
            let closure_register = compiler.declare_function_binding(class_name)?;
            compiler
                .instructions
                .push(Instruction::load_current_closure(closure_register));
        }
        let forwarded = ValueLocation::temp(compiler.alloc_temp());
        compiler
            .instructions
            .push(Instruction::call_super_forward(forwarded.register));
        if let Some(Binding::ThisRegister(this_register)) =
            compiler.scope.borrow().bindings.get("this").copied()
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

    /// §19.2.1 — Emits the directive-prologue string literals so their
    /// values land in the eval completion register. oxc lifts directive
    /// prologues out of the statement body and into `program.directives`,
    /// so without this step `eval("'hello'")` would return `undefined`
    /// instead of `"hello"`.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-performeval>
    pub(super) fn compile_eval_directive_completions(
        &mut self,
        directives: &[oxc_ast::ast::Directive<'_>],
    ) -> Result<(), SourceLoweringError> {
        debug_assert!(self.mode == LoweringMode::Eval && self.kind == FunctionKind::Script);
        for directive in directives {
            // Each directive's string literal evaluates to its cooked
            // value. We reuse the existing string-literal compile helper
            // to intern it, then copy the result into the completion
            // register.
            let lit = &directive.expression;
            let value = self.compile_string_literal(lit.value.as_str())?;
            let completion_reg = self.ensure_eval_completion_register()?;
            self.instructions
                .push(Instruction::move_(completion_reg, value.register));
            self.release(value);
        }
        Ok(())
    }

    fn compile_return_statement(
        &mut self,
        return_statement: &oxc_ast::ast::ReturnStatement<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        // §15.10.2 HasCallInTailPosition — in strict mode, the return
        // argument may contain a call in tail position (possibly nested
        // inside conditional, comma, or logical expressions).
        // Spec: <https://tc39.es/ecma262/#sec-static-semantics-hascallintailposition>
        if self.is_tail_call_eligible()
            && let Some(argument) = &return_statement.argument
            && Self::has_call_in_tail_position(argument)
        {
            return self.compile_return_with_tail_calls(argument, module);
        }

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

    /// Whether this function is eligible for tail calls at all.
    fn is_tail_call_eligible(&self) -> bool {
        self.strict_mode
            && self.finally_stack.is_empty()
            && !self.kind.is_generator()
            && !self.kind.is_async()
    }

    /// §15.10.2 HasCallInTailPosition — recursively check whether an
    /// expression contains a call in tail position.
    /// Spec: <https://tc39.es/ecma262/#sec-static-semantics-hascallintailposition>
    fn has_call_in_tail_position(expr: &Expression<'_>) -> bool {
        match expr {
            // §12.3.4.1 — `eval(...)` CAN be in tail position. The direct-eval
            // check (SameValue(func, %eval%)) is a *runtime* check, not
            // compile-time. When `eval` is rebound to a non-eval function the
            // call must be tail-call optimized. The TailCallClosure handler
            // falls back to a regular call for native callees, so this is safe.
            Expression::CallExpression(call) => {
                !matches!(&call.callee, Expression::Super(_))
                    && !call
                        .arguments
                        .iter()
                        .any(|arg| matches!(arg, Argument::SpreadElement(_)))
            }
            Expression::ConditionalExpression(cond) => {
                Self::has_call_in_tail_position(&cond.consequent)
                    || Self::has_call_in_tail_position(&cond.alternate)
            }
            Expression::SequenceExpression(seq) => seq
                .expressions
                .last()
                .is_some_and(|last| Self::has_call_in_tail_position(last)),
            Expression::LogicalExpression(logical) => {
                Self::has_call_in_tail_position(&logical.right)
            }
            Expression::ParenthesizedExpression(paren) => {
                Self::has_call_in_tail_position(&paren.expression)
            }
            Expression::TaggedTemplateExpression(_) => true,
            _ => false,
        }
    }

    /// Compile a return argument with tail-call awareness.
    /// Recursively walks conditional/comma/logical, emitting `TailCallClosure`
    /// for calls in tail position and `Return` for non-tail paths.
    fn compile_return_with_tail_calls(
        &mut self,
        expr: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        match expr {
            // Direct call in tail position (including `eval(...)` — see
            // has_call_in_tail_position for rationale).
            Expression::CallExpression(call)
                if !matches!(&call.callee, Expression::Super(_))
                    && !call
                        .arguments
                        .iter()
                        .any(|arg| matches!(arg, Argument::SpreadElement(_))) =>
            {
                self.emit_tail_call(call, module)
            }

            // §15.10.2 ConditionalExpression — both branches in tail position.
            Expression::ConditionalExpression(cond) => {
                let test = self.compile_expression(&cond.test, module)?;
                let jump_to_alt =
                    self.emit_conditional_placeholder(Opcode::JumpIfFalse, test.register);
                self.release(test);

                self.compile_return_with_tail_calls(&cond.consequent, module)?;
                let jump_to_end = self.emit_jump_placeholder();

                self.patch_jump(jump_to_alt, self.instructions.len())?;
                self.compile_return_with_tail_calls(&cond.alternate, module)?;

                self.patch_jump(jump_to_end, self.instructions.len())?;
                Ok(true)
            }

            // §15.10.2 SequenceExpression — last element in tail position.
            Expression::SequenceExpression(seq) => {
                let len = seq.expressions.len();
                for (i, sub_expr) in seq.expressions.iter().enumerate() {
                    if i == len - 1 {
                        return self.compile_return_with_tail_calls(sub_expr, module);
                    }
                    let val = self.compile_expression(sub_expr, module)?;
                    self.release(val);
                }
                let val = self.load_undefined()?;
                self.emit_iterator_closes_for_active_loops();
                self.instructions.push(Instruction::ret(val.register));
                self.release(val);
                Ok(true)
            }

            // §15.10.2 LogicalExpression — right operand in tail position.
            Expression::LogicalExpression(logical) => {
                self.compile_logical_tail_call(logical, module)
            }

            // Parenthesized — transparent to tail position.
            Expression::ParenthesizedExpression(paren) => {
                self.compile_return_with_tail_calls(&paren.expression, module)
            }

            // §15.10.2 TaggedTemplateExpression — tag call in tail position.
            Expression::TaggedTemplateExpression(tagged) => {
                self.emit_tail_call_tagged_template(tagged, module)
            }

            // Not a tail-call form — compile normally and return.
            _ => {
                let value = self.compile_expression(expr, module)?;
                self.emit_iterator_closes_for_active_loops();
                self.instructions.push(Instruction::ret(value.register));
                self.release(value);
                Ok(true)
            }
        }
    }

    /// Compile `return a OP b` where OP is &&, ||, ?? and `b` is in tail position.
    fn compile_logical_tail_call(
        &mut self,
        logical: &oxc_ast::ast::LogicalExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let left = self.compile_expression(&logical.left, module)?;
        let result_reg = if left.is_temp {
            left.register
        } else {
            let reg = self.alloc_temp();
            self.instructions
                .push(Instruction::move_(reg, left.register));
            reg
        };

        match logical.operator {
            LogicalOperator::And => {
                let jump_short = self.emit_conditional_placeholder(Opcode::JumpIfFalse, result_reg);
                // RHS is in tail position.
                self.compile_return_with_tail_calls(&logical.right, module)?;
                // Short-circuit: return LHS.
                self.patch_jump(jump_short, self.instructions.len())?;
                self.emit_iterator_closes_for_active_loops();
                self.instructions.push(Instruction::ret(result_reg));
            }
            LogicalOperator::Or => {
                let jump_short = self.emit_conditional_placeholder(Opcode::JumpIfTrue, result_reg);
                self.compile_return_with_tail_calls(&logical.right, module)?;
                self.patch_jump(jump_short, self.instructions.len())?;
                self.emit_iterator_closes_for_active_loops();
                self.instructions.push(Instruction::ret(result_reg));
            }
            LogicalOperator::Coalesce => {
                let null_val = self.load_null()?;
                let cmp = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::eq(
                    cmp.register,
                    result_reg,
                    null_val.register,
                ));
                self.release(null_val);
                let jump_if_null =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, cmp.register);

                let undef_val = self.load_undefined()?;
                self.instructions.push(Instruction::eq(
                    cmp.register,
                    result_reg,
                    undef_val.register,
                ));
                self.release(undef_val);
                let jump_if_undef =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, cmp.register);
                self.release(cmp);

                // Not nullish — return LHS.
                self.emit_iterator_closes_for_active_loops();
                self.instructions.push(Instruction::ret(result_reg));
                let jump_past_rhs = self.emit_jump_placeholder();

                // Nullish — RHS in tail position.
                let rhs_start = self.instructions.len();
                self.patch_jump(jump_if_null, rhs_start)?;
                self.patch_jump(jump_if_undef, rhs_start)?;
                self.compile_return_with_tail_calls(&logical.right, module)?;

                self.patch_jump(jump_past_rhs, self.instructions.len())?;
            }
        }
        Ok(true)
    }

    /// Emit a `TailCallClosure` instruction for a single call expression.
    fn emit_tail_call(
        &mut self,
        call: &oxc_ast::ast::CallExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let (callee, receiver) = self.compile_call_target(&call.callee, module)?;
        let receiver = match receiver {
            Some(receiver) if receiver.is_temp => Some(self.stabilize_binding_value(receiver)?),
            other => other,
        };
        let callee = if callee.is_temp {
            self.stabilize_binding_value(callee)?
        } else {
            callee
        };

        let argument_count = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::TooManyLocals)?;
        let mut argument_values = Vec::with_capacity(usize::from(argument_count));

        for argument in &call.arguments {
            let value = self.compile_expression(
                argument.as_expression().ok_or_else(|| {
                    SourceLoweringError::Unsupported("unsupported call argument".to_string())
                })?,
                module,
            )?;
            argument_values.push(if value.is_temp {
                self.stabilize_binding_value(value)?
            } else {
                value
            });
        }

        let arg_start = if argument_count == 0 {
            BytecodeRegister::new(self.next_local + self.next_temp)
        } else {
            self.reserve_temp_window(argument_count)?
        };

        for (offset, value) in argument_values.into_iter().enumerate() {
            let destination = BytecodeRegister::new(arg_start.index() + offset as u16);
            if value.register != destination {
                self.instructions
                    .push(Instruction::move_(destination, value.register));
                self.release(value);
            }
        }

        self.emit_iterator_closes_for_active_loops();

        let pc = self.instructions.len();
        self.instructions
            .push(Instruction::tail_call_closure(callee.register, arg_start));
        let call_site = match receiver {
            Some(receiver) => CallSite::Closure(ClosureCall::new_with_receiver(
                argument_count,
                FrameFlags::new(false, true, false),
                receiver.register,
            )),
            None => CallSite::Closure(ClosureCall::new(
                argument_count,
                FrameFlags::new(false, true, false),
            )),
        };
        self.record_call_site(pc, call_site);

        if argument_count != 0 {
            self.release_temp_window(argument_count.saturating_sub(1));
        }
        self.release(callee);
        if let Some(receiver) = receiver {
            self.release(receiver);
        }
        Ok(true)
    }

    /// Emit a `TailCallClosure` for a tagged template expression in tail position.
    /// Mirrors `compile_tagged_template_expression` but emits TailCallClosure
    /// instead of CallClosure at the end.
    fn emit_tail_call_tagged_template(
        &mut self,
        tagged: &oxc_ast::ast::TaggedTemplateExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<bool, SourceLoweringError> {
        let template = &tagged.quasi;

        // 1. Compile the tag expression (callee + optional receiver).
        let (callee, receiver) = self.compile_call_target(&tagged.tag, module)?;
        let receiver = match receiver {
            Some(r) if r.is_temp => Some(self.stabilize_binding_value(r)?),
            other => other,
        };
        let callee = if callee.is_temp {
            self.stabilize_binding_value(callee)?
        } else {
            callee
        };

        // 2. Build the template strings array (cooked values).
        let quasis_count = template.quasis.len() as u16;
        let strings_arr = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_array(strings_arr.register, quasis_count));
        let strings_arr = self.stabilize_binding_value(strings_arr)?;

        for (i, quasi) in template.quasis.iter().enumerate() {
            let cooked = quasi.value.cooked.as_ref().map(|s| s.as_str());
            let val = match cooked {
                Some(s) => self.compile_string_literal(s)?,
                None => {
                    let v = ValueLocation::temp(self.alloc_temp());
                    self.instructions
                        .push(Instruction::load_undefined(v.register));
                    v
                }
            };
            let idx = self.compile_numeric_literal(i as f64)?;
            self.instructions.push(Instruction::set_index(
                strings_arr.register,
                idx.register,
                val.register,
            ));
            self.release(idx);
            self.release(val);
        }

        // 3. Build the raw strings array.
        let raw_arr = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_array(raw_arr.register, quasis_count));
        let raw_arr = self.stabilize_binding_value(raw_arr)?;

        for (i, quasi) in template.quasis.iter().enumerate() {
            let raw_str = quasi.value.raw.as_str();
            let val = self.compile_string_literal(raw_str)?;
            let idx = self.compile_numeric_literal(i as f64)?;
            self.instructions.push(Instruction::set_index(
                raw_arr.register,
                idx.register,
                val.register,
            ));
            self.release(idx);
            self.release(val);
        }

        // 4. Set `strings.raw = rawArray`.
        let raw_prop = self.intern_property_name("raw")?;
        self.instructions.push(Instruction::set_property(
            strings_arr.register,
            raw_arr.register,
            raw_prop,
        ));
        self.release(raw_arr);

        // 5. Evaluate substitution expressions.
        let mut sub_values = Vec::with_capacity(template.expressions.len());
        for expr in &template.expressions {
            let val = self.compile_expression(expr, module)?;
            sub_values.push(if val.is_temp {
                self.stabilize_binding_value(val)?
            } else {
                val
            });
        }

        // 6. Emit TailCallClosure: tag(strings, sub0, sub1, ...)
        let total_args = 1 + sub_values.len();
        let argument_count =
            RegisterIndex::try_from(total_args).map_err(|_| SourceLoweringError::TooManyLocals)?;
        let arg_start = self.reserve_temp_window(argument_count)?;

        // First arg: template strings array.
        self.instructions
            .push(Instruction::move_(arg_start, strings_arr.register));
        self.release(strings_arr);

        // Remaining args: substitution values.
        for (i, val) in sub_values.into_iter().enumerate() {
            let dst = BytecodeRegister::new(arg_start.index() + 1 + i as u16);
            if val.register != dst {
                self.instructions
                    .push(Instruction::move_(dst, val.register));
                self.release(val);
            }
        }

        self.emit_iterator_closes_for_active_loops();

        let pc = self.instructions.len();
        self.instructions
            .push(Instruction::tail_call_closure(callee.register, arg_start));
        let call_site = match receiver {
            Some(recv) => CallSite::Closure(ClosureCall::new_with_receiver(
                argument_count,
                FrameFlags::new(false, true, false),
                recv.register,
            )),
            None => CallSite::Closure(ClosureCall::new(
                argument_count,
                FrameFlags::new(false, true, false),
            )),
        };
        self.record_call_site(pc, call_site);

        if argument_count != 0 {
            self.release_temp_window(argument_count.saturating_sub(1));
        }
        self.release(callee);
        if let Some(receiver) = receiver {
            self.release(receiver);
        }
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

    /// §15.4.4 MethodDefinition — class/object-literal method, getter, or
    /// setter. Not a constructor and has no `.prototype` own property.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-methoddefinitionevaluation>
    pub(super) fn emit_new_closure_method(
        &mut self,
        destination: BytecodeRegister,
        callee: FunctionIndex,
        explicit_captures: &[CaptureSource],
    ) -> Result<(), SourceLoweringError> {
        self.emit_new_closure_with_flags(
            destination,
            callee,
            explicit_captures,
            crate::object::ClosureFlags::method(),
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
        // Always use the explicit captures from the inner function's
        // compilation. The captures are descriptors expressed in *this*
        // (outer) function's scope: Register(R) means "copy this function's
        // local R into the new closure's upvalue slot", and Upvalue(I) means
        // "copy this function's upvalue I into the new closure's upvalue slot".
        //
        // We must NOT fall back to `self.captures` here: those are *this*
        // function's own captures, valid only when *this* closure was created
        // (in the grandparent context). Using them at a nested-closure site
        // would point at registers in a frame that does not exist.
        let captures = explicit_captures.to_vec();

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
        // §12.1.1 — In strict mode, `yield` and `let` are reserved and
        // cannot appear as assignment targets. oxc doesn't enforce this.
        if self.strict_mode && (name == "yield" || name == "let") {
            return Err(SourceLoweringError::EarlyError(format!(
                "'{name}' is a reserved identifier in strict mode"
            )));
        }
        match self.resolve_binding(name) {
            Ok(Binding::Register(register)) => {
                if register != value.register {
                    self.instructions
                        .push(Instruction::move_(register, value.register));
                    self.release(value);
                }
                Ok(ValueLocation::local(register))
            }
            Ok(Binding::Function {
                closure_register, ..
            }) => {
                if closure_register != value.register {
                    self.instructions
                        .push(Instruction::move_(closure_register, value.register));
                    self.release(value);
                }
                Ok(ValueLocation::local(closure_register))
            }
            Ok(Binding::Upvalue(upvalue)) => {
                self.instructions
                    .push(Instruction::set_upvalue(value.register, upvalue));
                Ok(value)
            }
            Ok(Binding::ThisRegister(register)) => {
                if register != value.register {
                    self.instructions
                        .push(Instruction::move_(register, value.register));
                    self.release(value);
                }
                Ok(ValueLocation::local(register))
            }
            Ok(Binding::ThisUpvalue(upvalue)) => {
                self.instructions
                    .push(Instruction::set_upvalue(value.register, upvalue));
                Ok(value)
            }
            // §15.7.15 — Immutable class name binding. Emit a runtime
            // TypeError throw so `assert.throws(TypeError, ...)` catches it.
            Ok(Binding::ImmutableRegister(_) | Binding::ImmutableUpvalue(_)) => {
                self.release(value);
                self.instructions.push(Instruction::throw_const_assign());
                // Unreachable after throw — return a dummy value for types.
                self.load_undefined()
            }
            // Undeclared identifier → assign to global property
            // Mirrors the read-path fallback in `compile_identifier`.
            // In strict mode, use SetGlobalStrict which throws ReferenceError
            // at runtime if the property does not already exist on global.
            // Spec: <https://tc39.es/ecma262/#sec-putvalue> step 5.a
            Err(SourceLoweringError::UnknownBinding(_)) => {
                let property = self.intern_property_name(name)?;
                if self.strict_mode {
                    self.instructions
                        .push(Instruction::set_global_strict(value.register, property));
                } else {
                    self.instructions
                        .push(Instruction::set_global(value.register, property));
                }
                Ok(value)
            }
            Err(e) => Err(e),
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
        if let Some(existing) = self.scope.borrow().bindings.get(name).copied() {
            // A pre-declared lexical placeholder (let/const/class hoisted by
            // `predeclare_function_scope`) gets reclaimed by the real
            // declaration here, regardless of `allow_redeclare`.
            if self.predeclared_lexical_names.remove(name)
                && let Binding::Register(register) = existing
            {
                return Ok(register);
            }

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
                Binding::ThisRegister(_)
                | Binding::Upvalue(_)
                | Binding::ThisUpvalue(_)
                | Binding::ImmutableRegister(_)
                | Binding::ImmutableUpvalue(_) => {
                    Err(SourceLoweringError::DuplicateBinding(name.to_string()))
                }
            };
        }

        let register = self.allocate_local()?;
        self.scope
            .borrow_mut()
            .bindings
            .insert(name.to_string(), Binding::Register(register));
        Ok(register)
    }

    pub(super) fn declare_function_binding(
        &mut self,
        name: &str,
    ) -> Result<BytecodeRegister, SourceLoweringError> {
        let closure_register =
            if let Some(existing) = self.scope.borrow().bindings.get(name).copied() {
                match existing {
                    Binding::Register(register) | Binding::ImmutableRegister(register) => register,
                    Binding::ThisRegister(register) => register,
                    Binding::Function {
                        closure_register, ..
                    } => closure_register,
                    Binding::Upvalue(_)
                    | Binding::ThisUpvalue(_)
                    | Binding::ImmutableUpvalue(_) => {
                        return Err(SourceLoweringError::Unsupported(format!(
                            "function declaration {name} conflicts with an upvalue binding"
                        )));
                    }
                }
            } else {
                self.allocate_local()?
            };

        self.scope
            .borrow_mut()
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
            .scope
            .borrow()
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
        if let Some(binding) = self.scope.borrow().bindings.get(name).copied() {
            return Ok(binding);
        }

        // §9.1.2 GetIdentifierReference: walk the full ancestor scope chain
        // (not just one level up). When the name is found at level `k`,
        // materialize implicit upvalues at every intermediate level
        // [k-1..0] AND in this function's own scope, so each closure
        // properly forwards the captured slot to the next.
        // Spec: <https://tc39.es/ecma262/#sec-getidentifierreference>
        let mut found_level: Option<usize> = None;
        let mut found_binding: Option<Binding> = None;
        for (level, parent_scope) in self.parent_scopes.iter().enumerate() {
            if let Some(binding) = parent_scope.borrow().bindings.get(name).copied() {
                found_level = Some(level);
                found_binding = Some(binding);
                break;
            }
        }

        if let (Some(level), Some(deepest_binding)) = (found_level, found_binding) {
            let is_this = matches!(
                deepest_binding,
                Binding::ThisRegister(_) | Binding::ThisUpvalue(_)
            );
            let is_immutable = matches!(
                deepest_binding,
                Binding::ImmutableRegister(_) | Binding::ImmutableUpvalue(_)
            );

            // Walk DOWN from the level just below where we found the binding
            // (level - 1) toward the immediate parent (0). At each step we
            // make sure that scope captures the name from the level above and
            // exposes it as a local upvalue binding so the next-deeper
            // closure can capture *that* upvalue.
            //
            // After this loop, `source_binding` is what the *current*
            // function should capture from `parent_scopes[0]` (its immediate
            // parent).
            let mut source_binding = deepest_binding;
            for inner_level in (0..level).rev() {
                let inner_scope = &self.parent_scopes[inner_level];
                let already = inner_scope.borrow().capture_ids.get(name).copied();
                let upvalue_id = if let Some(id) = already {
                    id
                } else {
                    let mut frame = inner_scope.borrow_mut();
                    let id = UpvalueId(
                        u16::try_from(frame.captures.len())
                            .map_err(|_| SourceLoweringError::TooManyLocals)?,
                    );
                    frame.captures.push(source_binding.capture_source());
                    frame.capture_ids.insert(name.to_string(), id);
                    let captured = if is_this {
                        Binding::ThisUpvalue(id)
                    } else if is_immutable {
                        Binding::ImmutableUpvalue(id)
                    } else {
                        Binding::Upvalue(id)
                    };
                    frame.bindings.insert(name.to_string(), captured);
                    id
                };
                source_binding = if is_this {
                    Binding::ThisUpvalue(upvalue_id)
                } else if is_immutable {
                    Binding::ImmutableUpvalue(upvalue_id)
                } else {
                    Binding::Upvalue(upvalue_id)
                };
            }

            // Materialize the upvalue in this function's own scope.
            let already_self = self.scope.borrow().capture_ids.get(name).copied();
            let upvalue = if let Some(existing) = already_self {
                existing
            } else {
                let mut scope = self.scope.borrow_mut();
                let id = UpvalueId(
                    u16::try_from(scope.captures.len())
                        .map_err(|_| SourceLoweringError::TooManyLocals)?,
                );
                scope.captures.push(source_binding.capture_source());
                scope.capture_ids.insert(name.to_string(), id);
                id
            };
            let captured = if is_this {
                Binding::ThisUpvalue(upvalue)
            } else if is_immutable {
                Binding::ImmutableUpvalue(upvalue)
            } else {
                Binding::Upvalue(upvalue)
            };
            self.scope
                .borrow_mut()
                .bindings
                .insert(name.to_string(), captured);
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
            let needs_create = !self.scope.borrow().bindings.contains_key("arguments");
            if needs_create {
                self.instructions
                    .push(Instruction::create_arguments(register));
                self.scope
                    .borrow_mut()
                    .bindings
                    .insert("arguments".to_string(), Binding::Register(register));
            }
            return Ok(Binding::Register(register));
        }

        Err(SourceLoweringError::UnknownBinding(name.to_string()))
    }

    /// Non-mutating check: is `name` visible as a local or captured (parent)
    /// binding? Used at compile time to detect whether `eval(...)` could be a
    /// direct eval — per §19.2.1.1, only references that resolve to %eval% are
    /// direct eval. If `eval` is locally rebound (e.g. `var eval = f`), the
    /// call is a regular call, not direct eval.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-function-calls-runtime-semantics-evaluation>
    pub(super) fn is_name_locally_visible(&self, name: &str) -> bool {
        if self.scope.borrow().bindings.contains_key(name) {
            return true;
        }
        for parent_scope in &self.parent_scopes {
            if parent_scope.borrow().bindings.contains_key(name) {
                return true;
            }
        }
        false
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

    /// Ensure the temp-region count `next_temp` is large enough that `register`
    /// falls within `[next_local, next_local + next_temp)`.
    ///
    /// Used when a helper returns a `ValueLocation::temp(register)` whose slot
    /// sits at a position higher than the current `next_temp` tracking. This
    /// happens in `compile_call_static_args` when the post-call stable register
    /// is higher than where intermediate releases left `next_temp`: without
    /// this correction, subsequent `alloc_temp` calls would hand out the same
    /// register as the live result and clobber it.
    pub(super) fn ensure_temp_region_covers(&mut self, register: BytecodeRegister) {
        let idx = register.index();
        if idx < self.next_local {
            return;
        }
        let required = idx - self.next_local + 1;
        if self.next_temp < required {
            self.next_temp = required;
            self.max_temp = self.max_temp.max(self.next_temp);
        }
    }

    fn declare_intrinsic_global_binding(&mut self, name: &str) -> Result<(), SourceLoweringError> {
        if self.scope.borrow().bindings.contains_key(name) {
            return Ok(());
        }

        let global = self.alloc_temp();
        self.instructions.push(Instruction::load_this(global));
        let binding = self.allocate_local()?;
        let property = self.intern_property_name(name)?;
        self.instructions
            .push(Instruction::get_property(binding, global, property));
        self.release(ValueLocation::temp(global));
        self.scope
            .borrow_mut()
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
        // Promote the value into a freshly-allocated local slot so the caller
        // can treat its register as stable across subsequent compile steps.
        //
        // `allocate_local()` returns `next_local`, which equals the register
        // index of the *lowest* currently-active temp. If the value is a
        // temp at that same slot we can repurpose it in place. Otherwise,
        // allocating a local there would alias with (and the subsequent
        // Move would clobber) a lower temp that outer callers still hold.
        //
        // To stay safe, fall back to allocating a fresh temp above all
        // current temps when there would be a collision. The temp is still
        // stable against LIFO discipline as long as the caller doesn't
        // release it. Returning a temp instead of a local is acceptable:
        // callers uniformly either release or ignore the returned value,
        // never treating it as a named local.
        if value.is_temp && value.register.index() == self.next_local {
            // Lowest temp: promote in place. No move, just reinterpret.
            let register = self.allocate_local()?;
            debug_assert_eq!(register.index(), value.register.index());
            self.release(value);
            return Ok(ValueLocation::local(register));
        }

        if self.next_temp == 0 {
            // No active temps — safe to allocate a fresh local.
            let register = self.allocate_local()?;
            if value.register != register {
                self.instructions
                    .push(Instruction::move_(register, value.register));
            }
            self.release(value);
            return Ok(ValueLocation::local(register));
        }

        // Collision risk: active temps exist and the value is not the
        // lowest. Snapshot to a fresh temp above the live temps. This is
        // stable as long as nothing releases it, which matches the caller's
        // expectation (they treat the returned value as immutable).
        let register = self.alloc_temp();
        if value.register != register {
            self.instructions
                .push(Instruction::move_(register, value.register));
        }
        // Note: if the input was a temp, we now have two active slots for it
        // (the original and the fresh snapshot). That's a one-register leak,
        // not a correctness bug — the original is still valid but unused.
        Ok(ValueLocation::temp(register))
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
                LoweringMode::Script | LoweringMode::Module => self.load_undefined()?,
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
            .push(crate::js_string::JsString::from_str(value));
        self.string_ids.insert(value.to_string(), id);
        Ok(id)
    }

    /// Interns a `JsString` (WTF-16) literal, preserving lone surrogates.
    pub(super) fn intern_js_string(
        &mut self,
        value: crate::js_string::JsString,
    ) -> Result<StringId, SourceLoweringError> {
        // For dedup, use a lossless key: format UTF-16 code units as hex.
        // This avoids collisions like 0xD800 vs 0xFFFD which both produce
        // the same lossy UTF-8 string (U+FFFD).
        let key = value
            .as_utf16()
            .iter()
            .map(|u| format!("{u:04x}"))
            .collect::<Vec<_>>()
            .join(":");
        let key = format!("wtf16:{key}");
        if let Some(existing) = self.string_ids.get(&key).copied() {
            return Ok(existing);
        }

        let id = StringId(
            u16::try_from(self.string_literals.len())
                .map_err(|_| SourceLoweringError::TooManyLocals)?,
        );
        self.string_literals.push(value);
        self.string_ids.insert(key, id);
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
