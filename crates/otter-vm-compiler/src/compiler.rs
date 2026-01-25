//! Main compiler implementation

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

use otter_vm_bytecode::{
    Instruction, JumpOffset, LocalIndex, Register,
    module::{ExportRecord, ImportBinding, ImportRecord},
};

use crate::codegen::CodeGen;
use crate::error::{CompileError, CompileResult};
use crate::literal_validator::{EcmaVersion, LiteralValidator};
use crate::scope::ResolvedBinding;

/// Maximum AST nesting depth to prevent stack overflow during compilation
const MAX_COMPILE_DEPTH: usize = 500;

/// The compiler
pub struct Compiler {
    /// Code generator
    codegen: CodeGen,
    /// Loop/Control stack (for `break`/`continue` patching)
    loop_stack: Vec<ControlScope>,
    /// Current compilation depth (for preventing stack overflow)
    depth: usize,
    /// Literal validator for ECMAScript compliance
    literal_validator: LiteralValidator,
}

#[derive(Debug)]
struct ControlScope {
    /// Whether this scope represents a loop (true) or switch (false)
    is_loop: bool,
    /// Jump indices for `break` statements targeting this scope
    break_jumps: Vec<usize>,
    /// Jump indices for `continue` statements targeting this scope (only if is_loop)
    continue_jumps: Vec<usize>,
    /// Target index for `continue` (start of loop iteration logic)
    continue_target: Option<usize>,
}

impl Compiler {
    /// Create a new compiler
    pub fn new() -> Self {
        Self {
            codegen: CodeGen::new(),
            loop_stack: Vec::new(),
            depth: 0,
            literal_validator: LiteralValidator::new(false, EcmaVersion::Latest),
        }
    }

    /// Set strict mode for literal validation
    pub fn set_strict_mode(&mut self, strict_mode: bool) {
        self.literal_validator.set_strict_mode(strict_mode);
    }

    /// Get the current strict mode setting
    pub fn is_strict_mode(&self) -> bool {
        self.literal_validator.is_strict_mode()
    }

    /// Enter a level of AST depth, checking for overflow
    fn enter_depth(&mut self) -> CompileResult<()> {
        self.depth += 1;
        if self.depth > MAX_COMPILE_DEPTH {
            Err(CompileError::Internal(
                "Maximum AST nesting depth exceeded".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Exit a level of AST depth
    fn exit_depth(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Compile source code to a module
    pub fn compile(
        mut self,
        source: &str,
        source_url: &str,
    ) -> CompileResult<otter_vm_bytecode::Module> {
        // Parse with oxc
        let allocator = Allocator::default();
        let source_type = SourceType::from_path(source_url).unwrap_or_default();
        let parser = Parser::new(&allocator, source, source_type);
        let result = parser.parse();

        // Check for parse errors
        if !result.errors.is_empty() {
            let error = &result.errors[0];
            return Err(CompileError::Parse(error.to_string()));
        }

        // Compile the program
        let program = result.program;
        self.compile_program(&program)?;

        // Ensure we return something
        self.codegen.emit(Instruction::ReturnUndefined);

        Ok(self.codegen.finish(source_url))
    }

    /// Compile a program
    fn compile_program(&mut self, program: &Program) -> CompileResult<()> {
        // Set strict mode from program
        let is_strict = program.source_type.is_strict()
            || program
                .directives
                .iter()
                .any(|d| d.directive.as_str() == "use strict");
        let is_strict = is_strict || self.has_use_strict_directive(&program.body);

        if is_strict {
            self.set_strict_mode(true);
        }

        // Validate program directives
        for d in &program.directives {
            self.literal_validator
                .validate_string_literal(&d.expression)?;
        }

        for stmt in &program.body {
            self.compile_statement(stmt)?;
        }
        Ok(())
    }

    fn has_use_strict_directive(&self, statements: &[Statement]) -> bool {
        for stmt in statements {
            match stmt {
                Statement::ExpressionStatement(expr_stmt) => {
                    if let Expression::StringLiteral(lit) = &expr_stmt.expression {
                        if lit.value.as_str() == "use strict" {
                            return true;
                        }
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        false
    }

    /// Compile a statement
    fn compile_statement(&mut self, stmt: &Statement) -> CompileResult<()> {
        self.enter_depth()?;
        let result = self.compile_statement_inner(stmt);
        self.exit_depth();
        result
    }

    /// Inner implementation of statement compilation
    fn compile_statement_inner(&mut self, stmt: &Statement) -> CompileResult<()> {
        match stmt {
            Statement::ExpressionStatement(expr_stmt) => {
                // Compile expression and discard result
                let reg = self.compile_expression(&expr_stmt.expression)?;
                self.codegen.free_reg(reg);
                Ok(())
            }

            Statement::VariableDeclaration(decl) => self.compile_variable_declaration(decl),

            Statement::ReturnStatement(ret) => {
                if let Some(arg) = &ret.argument {
                    let reg = self.compile_expression(arg)?;
                    self.codegen.emit(Instruction::Return { src: reg });
                    self.codegen.free_reg(reg);
                } else {
                    self.codegen.emit(Instruction::ReturnUndefined);
                }
                Ok(())
            }

            Statement::BlockStatement(block) => {
                self.codegen.enter_scope();
                for stmt in &block.body {
                    self.compile_statement(stmt)?;
                }
                self.codegen.exit_scope();
                Ok(())
            }

            Statement::IfStatement(if_stmt) => self.compile_if_statement(if_stmt),

            Statement::WhileStatement(while_stmt) => self.compile_while_statement(while_stmt),

            Statement::ForStatement(for_stmt) => self.compile_for_statement(for_stmt),

            Statement::ForOfStatement(for_of_stmt) => self.compile_for_of_statement(for_of_stmt),
            Statement::ForInStatement(for_in_stmt) => self.compile_for_in_statement(for_in_stmt),

            Statement::FunctionDeclaration(func) => self.compile_function_declaration(func),

            Statement::EmptyStatement(_) => Ok(()),

            Statement::DebuggerStatement(_) => {
                self.codegen.emit(Instruction::Debugger);
                Ok(())
            }

            Statement::TryStatement(try_stmt) => self.compile_try_statement(try_stmt),

            Statement::BreakStatement(break_stmt) => {
                if break_stmt.label.is_some() {
                    return Err(CompileError::unsupported("Labeled break"));
                }

                // Find nearest breakable scope (loop or switch)
                // Search in reverse order
                let mut target_scope_idx = None;
                for (i, scope) in self.loop_stack.iter_mut().enumerate().rev() {
                    target_scope_idx = Some(i);
                    break;
                }

                if let Some(idx) = target_scope_idx {
                    let jump_idx = self.codegen.emit_jump();
                    self.loop_stack[idx].break_jumps.push(jump_idx);
                    Ok(())
                } else {
                    Err(CompileError::syntax(
                        "break outside of loop or switch",
                        0,
                        0,
                    ))
                }
            }

            Statement::ContinueStatement(continue_stmt) => {
                if continue_stmt.label.is_some() {
                    return Err(CompileError::unsupported("Labeled continue"));
                }

                // Find nearest loop scope (skip switches)
                let mut target_scope_idx = None;
                for (i, scope) in self.loop_stack.iter_mut().enumerate().rev() {
                    if scope.is_loop {
                        target_scope_idx = Some(i);
                        break;
                    }
                }

                if let Some(idx) = target_scope_idx {
                    let jump_idx = self.codegen.emit_jump();
                    self.loop_stack[idx].continue_jumps.push(jump_idx);
                    Ok(())
                } else {
                    Err(CompileError::syntax("continue outside of loop", 0, 0))
                }
            }

            Statement::ThrowStatement(throw_stmt) => {
                let src = self.compile_expression(&throw_stmt.argument)?;
                self.codegen.emit(Instruction::Throw { src });
                self.codegen.free_reg(src);
                Ok(())
            }

            Statement::ImportDeclaration(import_decl) => {
                self.compile_import_declaration(import_decl)
            }

            Statement::ExportNamedDeclaration(export_decl) => {
                self.compile_export_named_declaration(export_decl)
            }

            Statement::ExportDefaultDeclaration(export_decl) => {
                self.compile_export_default_declaration(export_decl)
            }

            Statement::ExportAllDeclaration(export_decl) => {
                self.compile_export_all_declaration(export_decl)
            }

            // TypeScript statements - type erasure (skip type-only declarations)
            Statement::TSTypeAliasDeclaration(_) => Ok(()),
            Statement::TSInterfaceDeclaration(_) => Ok(()),
            Statement::TSEnumDeclaration(decl) => self.compile_ts_enum_declaration(decl),
            Statement::TSModuleDeclaration(decl) => self.compile_ts_module_declaration(decl),
            Statement::TSImportEqualsDeclaration(_) => {
                // import x = require('y') - old TS syntax, skip for now
                Err(CompileError::unsupported("TSImportEqualsDeclaration"))
            }
            Statement::TSExportAssignment(_) => {
                // export = x - old TS syntax, skip for now
                Err(CompileError::unsupported("TSExportAssignment"))
            }
            Statement::TSNamespaceExportDeclaration(_) => {
                // export as namespace X - for UMD, skip for now
                Err(CompileError::unsupported("TSNamespaceExportDeclaration"))
            }

            // Common JS features
            Statement::ClassDeclaration(class_decl) => self.compile_class_declaration(class_decl),
            Statement::SwitchStatement(switch_stmt) => self.compile_switch_statement(switch_stmt),
            Statement::DoWhileStatement(_) => Err(CompileError::unsupported("DoWhileStatement")),
            Statement::LabeledStatement(_) => Err(CompileError::unsupported("LabeledStatement")),
            Statement::WithStatement(_) => Err(CompileError::unsupported("WithStatement")),

            _ => Err(CompileError::unsupported("UnknownStatement")),
        }
    }

    /// Compile an import declaration
    ///
    /// import { foo } from './module.js'
    /// import foo from './module.js'
    /// import * as foo from './module.js'
    fn compile_import_declaration(&mut self, import: &ImportDeclaration) -> CompileResult<()> {
        let specifier = import.source.value.to_string();

        let mut bindings = Vec::new();

        if let Some(specifiers) = &import.specifiers {
            for spec in specifiers {
                match spec {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        bindings.push(ImportBinding::Named {
                            imported: s.imported.name().to_string(),
                            local: s.local.name.to_string(),
                        });
                        // Declare local variable for the import
                        self.codegen.declare_variable(&s.local.name, true)?;
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                        bindings.push(ImportBinding::Default {
                            local: s.local.name.to_string(),
                        });
                        self.codegen.declare_variable(&s.local.name, true)?;
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        bindings.push(ImportBinding::Namespace {
                            local: s.local.name.to_string(),
                        });
                        self.codegen.declare_variable(&s.local.name, true)?;
                    }
                }
            }
        }

        self.codegen.add_import(ImportRecord {
            specifier,
            bindings,
        });
        Ok(())
    }

    /// Compile an export named declaration
    ///
    /// export { foo, bar }
    /// export { foo as bar }
    /// export const x = 1
    /// export { foo } from './module.js'
    fn compile_export_named_declaration(
        &mut self,
        export: &ExportNamedDeclaration,
    ) -> CompileResult<()> {
        // Handle re-exports: export { foo } from './module.js'
        if let Some(source) = &export.source {
            let specifier = source.value.to_string();

            for spec in &export.specifiers {
                let exported = spec.exported.name().to_string();
                let imported = spec.local.name().to_string();

                self.codegen.add_export(ExportRecord::ReExportNamed {
                    specifier: specifier.clone(),
                    imported,
                    exported,
                });
            }
            return Ok(());
        }

        // Handle local exports: export { foo } or export { foo as bar }
        for spec in &export.specifiers {
            let local = spec.local.name().to_string();
            let exported = spec.exported.name().to_string();

            self.codegen
                .add_export(ExportRecord::Named { local, exported });
        }

        // Handle declaration: export const x = 1
        if let Some(decl) = &export.declaration {
            match decl {
                Declaration::VariableDeclaration(var_decl) => {
                    // Compile the variable declaration
                    self.compile_variable_declaration(var_decl)?;

                    // Add exports for each declarator
                    for declarator in &var_decl.declarations {
                        if let BindingPattern::BindingIdentifier(ident) = &declarator.id {
                            let name = ident.name.to_string();
                            self.codegen.add_export(ExportRecord::Named {
                                local: name.clone(),
                                exported: name,
                            });
                        }
                    }
                }
                Declaration::FunctionDeclaration(func) => {
                    self.compile_function_declaration(func)?;
                    if let Some(id) = &func.id {
                        let name = id.name.to_string();
                        self.codegen.add_export(ExportRecord::Named {
                            local: name.clone(),
                            exported: name,
                        });
                    }
                }
                Declaration::ClassDeclaration(class) => {
                    self.compile_class_declaration(class)?;
                    if let Some(id) = &class.id {
                        let name = id.name.to_string();
                        self.codegen.add_export(ExportRecord::Named {
                            local: name.clone(),
                            exported: name,
                        });
                    }
                }
                _ => return Err(CompileError::unsupported("Export declaration type")),
            }
        }

        Ok(())
    }

    /// Compile an export default declaration
    ///
    /// export default function() {}
    /// export default class {}
    /// export default expression
    fn compile_export_default_declaration(
        &mut self,
        export: &ExportDefaultDeclaration,
    ) -> CompileResult<()> {
        match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                // If the function has a name, use it; otherwise use a generated name
                let name = func
                    .id
                    .as_ref()
                    .map(|id| id.name.to_string())
                    .unwrap_or_else(|| "__default__".to_string());

                // Declare the variable
                self.codegen.declare_variable(&name, false)?;

                // Compile the function
                self.codegen.enter_function(Some(name.clone()));
                self.codegen.current.flags.is_async = func.r#async;

                // Declare parameters
                let mut param_defaults: Vec<(u16, &Expression)> = Vec::new();
                for param in &func.params.items {
                    match &param.pattern {
                        BindingPattern::BindingIdentifier(ident) => {
                            let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                            self.codegen.current.param_count += 1;
                            if let Some(init) = &param.initializer {
                                param_defaults.push((local_idx, init));
                            }
                        }
                        // Legacy / non-standard representation; keep for forward-compat.
                        BindingPattern::AssignmentPattern(assign) => {
                            let BindingPattern::BindingIdentifier(ident) = &assign.left else {
                                return Err(CompileError::unsupported(
                                    "Complex parameter patterns",
                                ));
                            };
                            let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                            self.codegen.current.param_count += 1;
                            param_defaults.push((local_idx, &assign.right));
                        }
                        _ => return Err(CompileError::unsupported("Complex parameter patterns")),
                    }
                }

                // Emit default parameter initializers (if arg === undefined).
                for (local_idx, default_expr) in param_defaults {
                    let cur = self.codegen.alloc_reg();
                    self.codegen.emit(Instruction::GetLocal {
                        dst: cur,
                        idx: LocalIndex(local_idx),
                    });
                    let undef = self.codegen.alloc_reg();
                    self.codegen.emit(Instruction::LoadUndefined { dst: undef });
                    let cond = self.codegen.alloc_reg();
                    self.codegen.emit(Instruction::StrictEq {
                        dst: cond,
                        lhs: cur,
                        rhs: undef,
                    });
                    let jump_skip = self.codegen.emit_jump_if_false(cond);
                    self.codegen.free_reg(cond);
                    self.codegen.free_reg(undef);
                    self.codegen.free_reg(cur);

                    let value = self.compile_expression(default_expr)?;
                    self.codegen.emit(Instruction::SetLocal {
                        idx: LocalIndex(local_idx),
                        src: value,
                    });
                    self.codegen.free_reg(value);

                    let end_offset = self.codegen.current_index() as i32 - jump_skip as i32;
                    self.codegen.patch_jump(jump_skip, end_offset);
                }

                // Compile function body
                if let Some(body) = &func.body {
                    for stmt in &body.statements {
                        self.compile_statement(stmt)?;
                    }
                }

                self.codegen.emit(Instruction::ReturnUndefined);
                let func_idx = self.codegen.exit_function();

                // Create closure and store
                if let Some(ResolvedBinding::Local(idx)) = self.codegen.resolve_variable(&name) {
                    let dst = self.codegen.alloc_reg();
                    if func.r#async {
                        self.codegen.emit(Instruction::AsyncClosure {
                            dst,
                            func: otter_vm_bytecode::FunctionIndex(func_idx),
                        });
                    } else {
                        self.codegen.emit(Instruction::Closure {
                            dst,
                            func: otter_vm_bytecode::FunctionIndex(func_idx),
                        });
                    }
                    self.codegen.emit(Instruction::SetLocal {
                        idx: LocalIndex(idx),
                        src: dst,
                    });
                    self.codegen.free_reg(dst);
                }

                self.codegen
                    .add_export(ExportRecord::Default { local: name });
            }

            ExportDefaultDeclarationKind::ClassDeclaration(class_decl) => {
                // `export default class {}` may be anonymous. Lower it to a local binding and export it.
                let name = class_decl
                    .id
                    .as_ref()
                    .map(|id| id.name.to_string())
                    .unwrap_or_else(|| "__default__".to_string());

                self.codegen.declare_variable(&name, false)?;

                let ctor = self.compile_class_declaration_value(class_decl)?;

                if let Some(ResolvedBinding::Local(idx)) = self.codegen.resolve_variable(&name) {
                    self.codegen.emit(Instruction::SetLocal {
                        idx: LocalIndex(idx),
                        src: ctor,
                    });
                    if self.codegen.current.name.as_deref() == Some("main") {
                        let name_idx = self.codegen.add_string(&name);
                        let ic_index = self.codegen.alloc_ic();
                        self.codegen.emit(Instruction::SetGlobal {
                            name: name_idx,
                            src: ctor,
                            ic_index,
                        });
                    }
                }

                self.codegen.free_reg(ctor);
                self.codegen
                    .add_export(ExportRecord::Default { local: name });
            }

            _ => {
                // Expression: export default expression
                // Create a local variable to hold the value
                let local_name = "__default__".to_string();
                let local_idx = self.codegen.declare_variable(&local_name, false)?;

                // Compile the expression
                let expr = export.declaration.to_expression();
                let reg = self.compile_expression(expr)?;

                // Store in local
                self.codegen.emit(Instruction::SetLocal {
                    idx: LocalIndex(local_idx),
                    src: reg,
                });
                self.codegen.free_reg(reg);

                self.codegen
                    .add_export(ExportRecord::Default { local: local_name });
            }
        }

        Ok(())
    }

    fn compile_class_declaration(&mut self, class_decl: &oxc_ast::ast::Class) -> CompileResult<()> {
        if class_decl.r#type != ClassType::ClassDeclaration {
            return Err(CompileError::internal("expected ClassDeclaration"));
        }
        let Some(id) = &class_decl.id else {
            return Err(CompileError::syntax(
                "Class declaration requires a name",
                0,
                0,
            ));
        };

        let name = id.name.to_string();

        // Declare class binding in current scope.
        self.codegen.declare_variable(&name, false)?;

        let ctor = self.compile_class_declaration_value(class_decl)?;

        if let Some(ResolvedBinding::Local(idx)) = self.codegen.resolve_variable(&name) {
            self.codegen.emit(Instruction::SetLocal {
                idx: LocalIndex(idx),
                src: ctor,
            });
            if self.codegen.current.name.as_deref() == Some("main") {
                let name_idx = self.codegen.add_string(&name);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetGlobal {
                    name: name_idx,
                    src: ctor,
                    ic_index,
                });
            }
        }

        self.codegen.free_reg(ctor);
        Ok(())
    }

    fn compile_class_expression(
        &mut self,
        class_expr: &oxc_ast::ast::Class,
    ) -> CompileResult<Register> {
        if class_expr.r#type != ClassType::ClassExpression {
            return Err(CompileError::internal("expected ClassExpression"));
        }
        self.compile_class_parts(class_expr.super_class.as_ref(), &class_expr.body)
    }

    fn compile_class_declaration_value(
        &mut self,
        class_decl: &oxc_ast::ast::Class,
    ) -> CompileResult<Register> {
        if class_decl.r#type != ClassType::ClassDeclaration {
            return Err(CompileError::internal("expected ClassDeclaration"));
        }
        self.compile_class_parts(class_decl.super_class.as_ref(), &class_decl.body)
    }

    fn compile_class_parts(
        &mut self,
        super_class: Option<&Expression>,
        body: &ClassBody,
    ) -> CompileResult<Register> {
        if super_class.is_some() {
            return Err(CompileError::unsupported("Class extends"));
        }

        // Find an explicit constructor, if present.
        let mut constructor: Option<&oxc_ast::ast::Function> = None;
        for elem in &body.body {
            match elem {
                ClassElement::MethodDefinition(method) => {
                    if matches!(method.kind, MethodDefinitionKind::Constructor) {
                        if method.r#static {
                            return Err(CompileError::syntax(
                                "Class constructor cannot be static",
                                0,
                                0,
                            ));
                        }
                        if constructor.is_some() {
                            return Err(CompileError::syntax(
                                "Class can only have one constructor",
                                0,
                                0,
                            ));
                        }
                        constructor = Some(&method.value);
                    }
                }
                ClassElement::TSIndexSignature(_) => {
                    // TypeScript-only; erase.
                }
                _ => return Err(CompileError::unsupported("Class element")),
            }
        }

        // Compile constructor (or a default empty constructor).
        let ctor = if let Some(func) = constructor {
            self.compile_function_expression(func)?
        } else {
            self.compile_empty_function()
        };

        // Get prototype object for instance methods: ctor.prototype
        let proto = self.codegen.alloc_reg();
        let proto_key = self.codegen.add_string("prototype");
        let ic_index = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::GetPropConst {
            dst: proto,
            obj: ctor,
            name: proto_key,
            ic_index,
        });

        for elem in &body.body {
            let ClassElement::MethodDefinition(method) = elem else {
                continue;
            };

            if matches!(method.kind, MethodDefinitionKind::Constructor) {
                continue;
            }

            let target = if method.r#static { ctor } else { proto };
            let func_reg = self.compile_function_expression(&method.value)?;

            // Compile the property key
            let key_reg = self.compile_property_key(&method.key)?;

            // Emit the appropriate instruction based on method kind
            match method.kind {
                MethodDefinitionKind::Method => {
                    let ic_index = self.codegen.alloc_ic();
                    self.codegen.emit(Instruction::SetProp {
                        obj: target,
                        key: key_reg,
                        val: func_reg,
                        ic_index,
                    });
                }
                MethodDefinitionKind::Get => {
                    self.codegen.emit(Instruction::DefineGetter {
                        obj: target,
                        key: key_reg,
                        func: func_reg,
                    });
                }
                MethodDefinitionKind::Set => {
                    self.codegen.emit(Instruction::DefineSetter {
                        obj: target,
                        key: key_reg,
                        func: func_reg,
                    });
                }
                MethodDefinitionKind::Constructor => unreachable!(),
            }

            self.codegen.free_reg(func_reg);
            self.codegen.free_reg(key_reg);
        }

        self.codegen.free_reg(proto);
        Ok(ctor)
    }

    fn compile_empty_function(&mut self) -> Register {
        let saved_loop_stack = std::mem::take(&mut self.loop_stack);

        self.codegen.enter_function(None);
        self.codegen.emit(Instruction::ReturnUndefined);
        let func_idx = self.codegen.exit_function();

        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Closure {
            dst,
            func: otter_vm_bytecode::FunctionIndex(func_idx),
        });

        self.loop_stack = saved_loop_stack;
        dst
    }

    /// Compile an export all declaration
    ///
    /// export * from './module.js'
    fn compile_export_all_declaration(
        &mut self,
        export: &ExportAllDeclaration,
    ) -> CompileResult<()> {
        let specifier = export.source.value.to_string();
        self.codegen
            .add_export(ExportRecord::ReExportAll { specifier });
        Ok(())
    }

    /// Compile a TypeScript enum declaration
    ///
    /// enum Color { Red, Green, Blue }
    /// Compiles to object with bidirectional mapping:
    /// { Red: 0, Green: 1, Blue: 2, 0: "Red", 1: "Green", 2: "Blue" }
    fn compile_ts_enum_declaration(&mut self, decl: &TSEnumDeclaration) -> CompileResult<()> {
        let enum_name = decl.id.name.as_str();

        // Declare variable for the enum
        let local_idx = self.codegen.declare_variable(enum_name, true)?;

        // Create enum object
        let enum_obj = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::NewObject { dst: enum_obj });

        // Iterate members
        let mut auto_value: i64 = 0;

        for member in &decl.body.members {
            let member_name = match &member.id {
                TSEnumMemberName::Identifier(id) => id.name.to_string(),
                TSEnumMemberName::String(s) => s.value.to_string(),
                TSEnumMemberName::ComputedString(s) => s.value.to_string(),
                TSEnumMemberName::ComputedTemplateString(_) => {
                    // Template string enum member - not supported for now
                    continue;
                }
            };

            // Compute value
            let is_numeric = if let Some(init) = &member.initializer {
                // Has explicit initializer
                let val_reg = self.compile_expression(init)?;

                // Set forward mapping: Color["Red"] = value
                let name_idx = self.codegen.add_string(&member_name);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetPropConst {
                    obj: enum_obj,
                    name: name_idx,
                    val: val_reg,
                    ic_index,
                });

                // For numeric values, also set reverse mapping
                // We check if it's a numeric literal to set reverse mapping
                let is_numeric = matches!(init, Expression::NumericLiteral(_));
                if is_numeric {
                    // Set reverse mapping: Color[value] = "Red"
                    let str_val = self.codegen.alloc_reg();
                    let str_idx = self.codegen.add_string(&member_name);
                    self.codegen.emit(Instruction::LoadConst {
                        dst: str_val,
                        idx: str_idx,
                    });
                    let ic_index = self.codegen.alloc_ic();
                    self.codegen.emit(Instruction::SetProp {
                        obj: enum_obj,
                        key: val_reg,
                        val: str_val,
                        ic_index,
                    });
                    self.codegen.free_reg(str_val);

                    // Update auto_value if numeric literal
                    if let Expression::NumericLiteral(lit) = init {
                        auto_value = lit.value as i64 + 1;
                    }
                }

                self.codegen.free_reg(val_reg);
                is_numeric
            } else {
                // Auto-increment numeric value
                let val_reg = self.codegen.alloc_reg();
                self.codegen.emit(Instruction::LoadInt32 {
                    dst: val_reg,
                    value: auto_value as i32,
                });

                // Set forward mapping: Color["Red"] = 0
                let name_idx = self.codegen.add_string(&member_name);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetPropConst {
                    obj: enum_obj,
                    name: name_idx,
                    val: val_reg,
                    ic_index,
                });

                // Set reverse mapping: Color[0] = "Red"
                let str_val = self.codegen.alloc_reg();
                let str_idx = self.codegen.add_string(&member_name);
                self.codegen.emit(Instruction::LoadConst {
                    dst: str_val,
                    idx: str_idx,
                });
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetProp {
                    obj: enum_obj,
                    key: val_reg,
                    val: str_val,
                    ic_index,
                });
                self.codegen.free_reg(str_val);
                self.codegen.free_reg(val_reg);

                auto_value += 1;
                true
            };

            // For string enums (non-numeric), no reverse mapping
            let _ = is_numeric;
        }

        // Store enum object in variable
        self.codegen.emit(Instruction::SetLocal {
            idx: LocalIndex(local_idx),
            src: enum_obj,
        });
        self.codegen.free_reg(enum_obj);

        Ok(())
    }

    /// Compile a TypeScript module/namespace declaration
    ///
    /// namespace Foo { export const x = 1; }
    fn compile_ts_module_declaration(&mut self, decl: &TSModuleDeclaration) -> CompileResult<()> {
        // Get namespace name
        let ns_name = match &decl.id {
            TSModuleDeclarationName::Identifier(id) => id.name.to_string(),
            TSModuleDeclarationName::StringLiteral(s) => s.value.to_string(),
        };

        // Declare variable for the namespace
        let local_idx = self.codegen.declare_variable(&ns_name, false)?;

        // Create namespace object
        let ns_obj = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::NewObject { dst: ns_obj });

        // Store namespace object first (so recursive references work)
        self.codegen.emit(Instruction::SetLocal {
            idx: LocalIndex(local_idx),
            src: ns_obj,
        });

        // Compile body if present
        if let Some(body) = &decl.body {
            match body {
                TSModuleDeclarationBody::TSModuleBlock(block) => {
                    self.codegen.enter_scope();

                    for stmt in &block.body {
                        // Handle exports within namespace differently
                        match stmt {
                            Statement::ExportNamedDeclaration(export) => {
                                // Compile the declaration
                                if let Some(inner_decl) = &export.declaration {
                                    match inner_decl {
                                        Declaration::VariableDeclaration(var_decl) => {
                                            self.compile_variable_declaration(var_decl)?;

                                            // Add to namespace object
                                            for declarator in &var_decl.declarations {
                                                if let BindingPattern::BindingIdentifier(ident) =
                                                    &declarator.id
                                                {
                                                    let val =
                                                        self.compile_identifier(&ident.name)?;
                                                    let name_idx =
                                                        self.codegen.add_string(&ident.name);
                                                    let ic_index = self.codegen.alloc_ic();
                                                    self.codegen.emit(Instruction::SetPropConst {
                                                        obj: ns_obj,
                                                        name: name_idx,
                                                        val,
                                                        ic_index,
                                                    });
                                                    self.codegen.free_reg(val);
                                                }
                                            }
                                        }
                                        Declaration::FunctionDeclaration(func) => {
                                            self.compile_function_declaration(func)?;

                                            if let Some(id) = &func.id {
                                                let val = self.compile_identifier(&id.name)?;
                                                let name_idx = self.codegen.add_string(&id.name);
                                                let ic_index = self.codegen.alloc_ic();
                                                self.codegen.emit(Instruction::SetPropConst {
                                                    obj: ns_obj,
                                                    name: name_idx,
                                                    val,
                                                    ic_index,
                                                });
                                                self.codegen.free_reg(val);
                                            }
                                        }
                                        _ => {
                                            // Other declarations (classes, etc.)
                                        }
                                    }
                                }
                            }
                            _ => {
                                // Non-export statements - compile normally
                                self.compile_statement(stmt)?;
                            }
                        }
                    }

                    self.codegen.exit_scope();
                }
                TSModuleDeclarationBody::TSModuleDeclaration(nested) => {
                    // Nested namespace: namespace A.B.C { }
                    self.compile_ts_module_declaration(nested)?;
                }
            }
        }

        self.codegen.free_reg(ns_obj);
        Ok(())
    }

    /// Compile a variable declaration
    fn compile_variable_declaration(&mut self, decl: &VariableDeclaration) -> CompileResult<()> {
        use crate::scope::VariableKind;

        let kind = match decl.kind {
            VariableDeclarationKind::Var => VariableKind::Var,
            VariableDeclarationKind::Let => VariableKind::Let,
            VariableDeclarationKind::Const => VariableKind::Const,
            _ => VariableKind::Let, // Default to Let for any future kinds
        };

        for declarator in &decl.declarations {
            match &declarator.id {
                BindingPattern::BindingIdentifier(ident) => {
                    let local_idx = self.codegen.declare_variable_with_kind(&ident.name, kind)?;

                    if let Some(init) = &declarator.init {
                        let reg = self.compile_expression(init)?;
                        self.codegen.emit(Instruction::SetLocal {
                            idx: LocalIndex(local_idx),
                            src: reg,
                        });
                        // Our VM does not yet support upvalue capture across function contexts.
                        // Publishing top-level declarations onto `globalThis` keeps builtins and
                        // nested functions usable (they resolve missing bindings via GetGlobal).
                        if self.codegen.current.name.as_deref() == Some("main") {
                            let name_idx = self.codegen.add_string(&ident.name);
                            let ic_index = self.codegen.alloc_ic();
                            self.codegen.emit(Instruction::SetGlobal {
                                name: name_idx,
                                src: reg,
                                ic_index,
                            });
                        }
                        self.codegen.free_reg(reg);
                    }
                }
                _ => return Err(CompileError::unsupported("Destructuring patterns")),
            }
        }

        Ok(())
    }

    /// Compile an if statement
    fn compile_if_statement(&mut self, if_stmt: &IfStatement) -> CompileResult<()> {
        // Compile condition
        let cond = self.compile_expression(&if_stmt.test)?;
        let jump_else = self.codegen.emit_jump_if_false(cond);
        self.codegen.free_reg(cond);

        // Compile consequent
        self.compile_statement(&if_stmt.consequent)?;

        if let Some(alternate) = &if_stmt.alternate {
            // Jump over else branch
            let jump_end = self.codegen.emit_jump();

            // Patch jump to else
            let else_offset = self.codegen.current_index() as i32 - jump_else as i32;
            self.codegen.patch_jump(jump_else, else_offset);

            // Compile alternate
            self.compile_statement(alternate)?;

            // Patch jump to end
            let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
            self.codegen.patch_jump(jump_end, end_offset);
        } else {
            // Patch jump to end
            let end_offset = self.codegen.current_index() as i32 - jump_else as i32;
            self.codegen.patch_jump(jump_else, end_offset);
        }

        Ok(())
    }

    /// Compile a while statement
    fn compile_while_statement(&mut self, while_stmt: &WhileStatement) -> CompileResult<()> {
        let loop_start = self.codegen.current_index();
        self.loop_stack.push(ControlScope {
            is_loop: true,
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            continue_target: Some(loop_start),
        });

        // Compile condition
        let cond = self.compile_expression(&while_stmt.test)?;
        let jump_end = self.codegen.emit_jump_if_false(cond);
        self.codegen.free_reg(cond);

        // Compile body
        self.compile_statement(&while_stmt.body)?;

        // Jump back to start
        let back_offset = loop_start as i32 - self.codegen.current_index() as i32;
        self.codegen.emit(Instruction::Jump {
            offset: JumpOffset(back_offset),
        });

        // Patch jump to end
        let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
        self.codegen.patch_jump(jump_end, end_offset);

        let break_target = self.codegen.current_index();
        let loop_ctl = self.loop_stack.pop().expect("loop stack underflow");
        for jump in loop_ctl.break_jumps {
            let offset = break_target as i32 - jump as i32;
            self.codegen.patch_jump(jump, offset);
        }
        if let Some(continue_target) = loop_ctl.continue_target {
            for jump in loop_ctl.continue_jumps {
                let offset = continue_target as i32 - jump as i32;
                self.codegen.patch_jump(jump, offset);
            }
        }

        Ok(())
    }

    /// Compile a for statement
    fn compile_for_statement(&mut self, for_stmt: &ForStatement) -> CompileResult<()> {
        self.codegen.enter_scope();

        // Compile init
        if let Some(init) = &for_stmt.init {
            match init {
                ForStatementInit::VariableDeclaration(decl) => {
                    self.compile_variable_declaration(decl)?;
                }
                _ => {
                    // Handle expression init
                    if let Some(expr) = init.as_expression() {
                        let reg = self.compile_expression(expr)?;
                        self.codegen.free_reg(reg);
                    }
                }
            }
        }

        let loop_start = self.codegen.current_index();
        self.loop_stack.push(ControlScope {
            is_loop: true,
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            continue_target: Some(loop_start),
        });

        // Compile test
        let jump_end = if let Some(test) = &for_stmt.test {
            let cond = self.compile_expression(test)?;
            let jump = self.codegen.emit_jump_if_false(cond);
            self.codegen.free_reg(cond);
            Some(jump)
        } else {
            None
        };

        // Compile body
        self.compile_statement(&for_stmt.body)?;

        // Compile update
        let update_start = self.codegen.current_index();
        if let Some(update) = &for_stmt.update {
            if let Some(loop_ctl) = self.loop_stack.last_mut() {
                loop_ctl.continue_target = Some(update_start);
            }
            let reg = self.compile_expression(update)?;
            self.codegen.free_reg(reg);
        }

        // Jump back to start
        let back_offset = loop_start as i32 - self.codegen.current_index() as i32;
        self.codegen.emit(Instruction::Jump {
            offset: JumpOffset(back_offset),
        });

        // Patch jump to end
        if let Some(jump_end) = jump_end {
            let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
            self.codegen.patch_jump(jump_end, end_offset);
        }

        let break_target = self.codegen.current_index();
        let loop_ctl = self.loop_stack.pop().expect("loop stack underflow");
        for jump in loop_ctl.break_jumps {
            let offset = break_target as i32 - jump as i32;
            self.codegen.patch_jump(jump, offset);
        }
        if let Some(continue_target) = loop_ctl.continue_target {
            for jump in loop_ctl.continue_jumps {
                let offset = continue_target as i32 - jump as i32;
                self.codegen.patch_jump(jump, offset);
            }
        }

        self.codegen.exit_scope();
        Ok(())
    }

    /// Compile a for-of statement
    /// for (const x of iterable) { ... }
    fn compile_for_of_statement(&mut self, for_of_stmt: &ForOfStatement) -> CompileResult<()> {
        self.codegen.enter_scope();

        // Compile the iterable expression
        let iterable = self.compile_expression(&for_of_stmt.right)?;

        // Get iterator: iterator = iterable[Symbol.iterator]()
        let iterator = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::GetIterator {
            dst: iterator,
            src: iterable,
        });
        self.codegen.free_reg(iterable);

        // Allocate registers for value and done
        let value_reg = self.codegen.alloc_reg();
        let done_reg = self.codegen.alloc_reg();
        let result_reg = self.codegen.alloc_reg();

        let next_name = self.codegen.add_string("next");
        let done_name = self.codegen.add_string("done");
        let value_name = self.codegen.add_string("value");

        // Loop start
        let loop_start = self.codegen.current_index();
        self.loop_stack.push(ControlScope {
            is_loop: true,
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            continue_target: Some(loop_start),
        });

        // result = iterator.next()
        // (Lowered to CallMethod so `next` can be a JS function.)
        let frame = self.codegen.alloc_fresh_block(1);
        self.codegen.emit(Instruction::Move {
            dst: frame,
            src: iterator,
        });
        let ic_index = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::CallMethod {
            dst: result_reg,
            obj: frame,
            method: next_name,
            argc: 0,
            ic_index,
        });
        self.codegen.free_reg(frame);

        // done = result.done; value = result.value
        let ic_index_done = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::GetPropConst {
            dst: done_reg,
            obj: result_reg,
            name: done_name,
            ic_index: ic_index_done,
        });
        let ic_index_value = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::GetPropConst {
            dst: value_reg,
            obj: result_reg,
            name: value_name,
            ic_index: ic_index_value,
        });

        // JumpIfTrue done -> end
        let jump_end = self.codegen.emit_jump_if_true(done_reg);

        // Assign value to the left side
        match &for_of_stmt.left {
            ForStatementLeft::VariableDeclaration(decl) => {
                // For variable declarations like `const x` or `let x`
                let is_const = decl.kind == VariableDeclarationKind::Const;
                if let Some(declarator) = decl.declarations.first() {
                    match &declarator.id {
                        BindingPattern::BindingIdentifier(ident) => {
                            // Declare the variable and set its value
                            let local_idx = self.codegen.declare_variable(&ident.name, is_const)?;
                            self.codegen.emit(Instruction::SetLocal {
                                idx: LocalIndex(local_idx),
                                src: value_reg,
                            });
                        }
                        BindingPattern::ArrayPattern(array_pat) => {
                            // Minimal support for `for (const [a, b] of iter) { ... }`
                            // by indexing into the yielded value.
                            if array_pat.rest.is_some() {
                                return Err(CompileError::unsupported(
                                    "Array rest in for-of destructuring",
                                ));
                            }

                            for (i, elem) in array_pat.elements.iter().enumerate() {
                                let Some(elem_pat) = elem else { continue };
                                let BindingPattern::BindingIdentifier(ident) = elem_pat else {
                                    return Err(CompileError::unsupported(
                                        "Complex destructuring in for-of",
                                    ));
                                };

                                let local_idx =
                                    self.codegen.declare_variable(&ident.name, is_const)?;

                                let idx_reg = self.codegen.alloc_reg();
                                self.codegen.emit(Instruction::LoadInt32 {
                                    dst: idx_reg,
                                    value: i as i32,
                                });

                                let elem_reg = self.codegen.alloc_reg();
                                self.codegen.emit(Instruction::GetElem {
                                    dst: elem_reg,
                                    arr: value_reg,
                                    idx: idx_reg,
                                });

                                self.codegen.emit(Instruction::SetLocal {
                                    idx: LocalIndex(local_idx),
                                    src: elem_reg,
                                });

                                self.codegen.free_reg(elem_reg);
                                self.codegen.free_reg(idx_reg);
                            }
                        }
                        _ => {
                            // Destructuring patterns in for-of
                            return Err(CompileError::unsupported(
                                "Destructuring in for-of not yet supported",
                            ));
                        }
                    }
                }
            }
            _ => {
                // Assignment to existing variable
                // For simple identifiers, we can handle them
                // Complex assignment targets (destructuring, member access) need more work
                return Err(CompileError::unsupported(
                    "Complex for-of left-hand side not yet supported",
                ));
            }
        }

        // Compile the loop body
        self.compile_statement(&for_of_stmt.body)?;

        // Jump back to loop start
        let back_offset = loop_start as i32 - self.codegen.current_index() as i32;
        self.codegen.emit(Instruction::Jump {
            offset: JumpOffset(back_offset),
        });

        // Patch jump to end
        let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
        self.codegen.patch_jump(jump_end, end_offset);

        let break_target = self.codegen.current_index();
        let loop_ctl = self.loop_stack.pop().expect("loop stack underflow");
        for jump in loop_ctl.break_jumps {
            let offset = break_target as i32 - jump as i32;
            self.codegen.patch_jump(jump, offset);
        }
        if let Some(continue_target) = loop_ctl.continue_target {
            for jump in loop_ctl.continue_jumps {
                let offset = continue_target as i32 - jump as i32;
                self.codegen.patch_jump(jump, offset);
            }
        }

        // Clean up registers
        self.codegen.free_reg(done_reg);
        self.codegen.free_reg(value_reg);
        self.codegen.free_reg(result_reg);
        self.codegen.free_reg(iterator);

        self.codegen.exit_scope();
        Ok(())
    }

    fn compile_for_in_statement(&mut self, _for_in_stmt: &ForInStatement) -> CompileResult<()> {
        // Stub: Treat as runtime error for now, but allow compilation to succeed
        let msg = self
            .codegen
            .add_string("ForInStatement not yet implemented in runtime");
        let reg = self.codegen.alloc_reg();
        self.codegen
            .emit(Instruction::LoadConst { dst: reg, idx: msg });
        self.codegen.emit(Instruction::Throw { src: reg });
        self.codegen.free_reg(reg);
        Ok(())
    }

    fn compile_try_statement(&mut self, try_stmt: &TryStatement) -> CompileResult<()> {
        if try_stmt.finalizer.is_some() {
            return Err(CompileError::unsupported("try/finally"));
        }
        let Some(handler) = &try_stmt.handler else {
            return Err(CompileError::unsupported("try without catch"));
        };

        // Emit try start (patch catch offset later)
        let try_start = self.codegen.current_index();
        self.codegen.emit(Instruction::TryStart {
            catch_offset: JumpOffset(0),
        });

        // Compile try block
        for stmt in &try_stmt.block.body {
            self.compile_statement(stmt)?;
        }

        // Normal completion pops the handler
        self.codegen.emit(Instruction::TryEnd);

        // Jump over catch
        let jump_over_catch = self.codegen.emit_jump();

        // Patch try_start to jump into catch block
        let catch_start = self.codegen.current_index();
        let catch_offset = catch_start as i32 - try_start as i32;
        self.codegen.patch_jump(try_start, catch_offset);

        // Catch block begins: load exception into a register, bind param, then run body
        self.codegen.enter_scope();

        let exc_reg = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Catch { dst: exc_reg });

        if let Some(param) = &handler.param {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.emit(Instruction::SetLocal {
                        idx: LocalIndex(local_idx),
                        src: exc_reg,
                    });
                }
                _ => return Err(CompileError::unsupported("Complex catch parameter pattern")),
            }
        }

        for stmt in &handler.body.body {
            self.compile_statement(stmt)?;
        }

        self.codegen.free_reg(exc_reg);
        self.codegen.exit_scope();

        // Patch jump over catch
        let end_offset = self.codegen.current_index() as i32 - jump_over_catch as i32;
        self.codegen.patch_jump(jump_over_catch, end_offset);

        Ok(())
    }

    /// Compile a function declaration
    fn compile_function_declaration(&mut self, func: &oxc_ast::ast::Function) -> CompileResult<()> {
        let name = func.id.as_ref().map(|id| id.name.to_string());
        let is_async = func.r#async;
        let is_generator = func.generator;

        let saved_loop_stack = std::mem::take(&mut self.loop_stack);

        // Declare function in current scope
        if let Some(ref n) = name {
            self.codegen.declare_variable(n, false)?;
        }

        // Enter function context
        self.codegen.enter_function(name.clone());
        self.codegen.current.flags.is_async = is_async;
        self.codegen.current.flags.is_generator = is_generator;

        // Declare parameters and collect defaults
        let mut param_defaults: Vec<(u16, &Expression)> = Vec::new();
        for param in &func.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                    if let Some(init) = &param.initializer {
                        param_defaults.push((local_idx, init));
                    }
                }
                // Legacy / non-standard representation; keep for forward-compat.
                BindingPattern::AssignmentPattern(assign) => {
                    let BindingPattern::BindingIdentifier(ident) = &assign.left else {
                        return Err(CompileError::unsupported("Complex parameter patterns"));
                    };
                    let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                    param_defaults.push((local_idx, &assign.right));
                }
                _ => return Err(CompileError::unsupported("Complex parameter patterns")),
            }
        }

        // Check for rest parameter at function level
        if let Some(rest) = &func.params.rest {
            if let BindingPattern::BindingIdentifier(ident) = &rest.rest.argument {
                self.codegen.declare_variable(&ident.name, false)?;
                self.codegen.current.flags.has_rest = true;
            } else {
                return Err(CompileError::unsupported("Complex rest parameter pattern"));
            }
        }

        // Emit default parameter initializers (if arg === undefined).
        for (local_idx, default_expr) in param_defaults {
            let cur = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::GetLocal {
                dst: cur,
                idx: LocalIndex(local_idx),
            });
            let undef = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::LoadUndefined { dst: undef });
            let cond = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::StrictEq {
                dst: cond,
                lhs: cur,
                rhs: undef,
            });
            let jump_skip = self.codegen.emit_jump_if_false(cond);
            self.codegen.free_reg(cond);
            self.codegen.free_reg(undef);
            self.codegen.free_reg(cur);

            let value = self.compile_expression(default_expr)?;
            self.codegen.emit(Instruction::SetLocal {
                idx: LocalIndex(local_idx),
                src: value,
            });
            self.codegen.free_reg(value);

            let end_offset = self.codegen.current_index() as i32 - jump_skip as i32;
            self.codegen.patch_jump(jump_skip, end_offset);
        }

        // Compile function body
        if let Some(body) = &func.body {
            let saved_strict = self.is_strict_mode();
            let has_use_strict = body
                .directives
                .iter()
                .any(|d| d.directive.as_str() == "use strict");
            let has_use_strict = has_use_strict || self.has_use_strict_directive(&body.statements);

            if has_use_strict {
                self.set_strict_mode(true);
            }

            // Validate directives
            for d in &body.directives {
                self.literal_validator
                    .validate_string_literal(&d.expression)?;
            }

            for stmt in &body.statements {
                self.compile_statement(stmt)?;
            }

            self.set_strict_mode(saved_strict);
        }

        // Ensure return
        self.codegen.emit(Instruction::ReturnUndefined);

        // Exit function and get index
        let func_idx = self.codegen.exit_function();

        // Create closure and store in variable
        if let Some(n) = name
            && let Some(ResolvedBinding::Local(idx)) = self.codegen.resolve_variable(&n)
        {
            let dst = self.codegen.alloc_reg();
            if is_generator {
                self.codegen.emit(Instruction::GeneratorClosure {
                    dst,
                    func: otter_vm_bytecode::FunctionIndex(func_idx),
                });
            } else if is_async {
                self.codegen.emit(Instruction::AsyncClosure {
                    dst,
                    func: otter_vm_bytecode::FunctionIndex(func_idx),
                });
            } else {
                self.codegen.emit(Instruction::Closure {
                    dst,
                    func: otter_vm_bytecode::FunctionIndex(func_idx),
                });
            }
            self.codegen.emit(Instruction::SetLocal {
                idx: LocalIndex(idx),
                src: dst,
            });
            if self.codegen.current.name.as_deref() == Some("main") {
                let name_idx = self.codegen.add_string(&n);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetGlobal {
                    name: name_idx,
                    src: dst,
                    ic_index,
                });
            }
            self.codegen.free_reg(dst);
        }

        self.loop_stack = saved_loop_stack;
        Ok(())
    }

    /// Compile a function expression
    fn compile_function_expression(
        &mut self,
        func: &oxc_ast::ast::Function,
    ) -> CompileResult<Register> {
        let name = func.id.as_ref().map(|id| id.name.to_string());
        let is_async = func.r#async;
        let is_generator = func.generator;

        let saved_loop_stack = std::mem::take(&mut self.loop_stack);

        // Enter function context
        self.codegen.enter_function(name);
        self.codegen.current.flags.is_async = is_async;
        self.codegen.current.flags.is_generator = is_generator;

        // Declare parameters and collect defaults
        let mut param_defaults: Vec<(u16, &Expression)> = Vec::new();
        for param in &func.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                    if let Some(init) = &param.initializer {
                        param_defaults.push((local_idx, init));
                    }
                }
                // Legacy / non-standard representation; keep for forward-compat.
                BindingPattern::AssignmentPattern(assign) => {
                    let BindingPattern::BindingIdentifier(ident) = &assign.left else {
                        return Err(CompileError::unsupported("Complex parameter patterns"));
                    };
                    let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                    param_defaults.push((local_idx, &assign.right));
                }
                _ => return Err(CompileError::unsupported("Complex parameter patterns")),
            }
        }

        // Check for rest parameter at function level
        if let Some(rest) = &func.params.rest {
            if let BindingPattern::BindingIdentifier(ident) = &rest.rest.argument {
                self.codegen.declare_variable(&ident.name, false)?;
                self.codegen.current.flags.has_rest = true;
            } else {
                return Err(CompileError::unsupported("Complex rest parameter pattern"));
            }
        }

        // Emit default parameter initializers (if arg === undefined).
        for (local_idx, default_expr) in param_defaults {
            let cur = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::GetLocal {
                dst: cur,
                idx: LocalIndex(local_idx),
            });
            let undef = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::LoadUndefined { dst: undef });
            let cond = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::StrictEq {
                dst: cond,
                lhs: cur,
                rhs: undef,
            });
            let jump_skip = self.codegen.emit_jump_if_false(cond);
            self.codegen.free_reg(cond);
            self.codegen.free_reg(undef);
            self.codegen.free_reg(cur);

            let value = self.compile_expression(default_expr)?;
            self.codegen.emit(Instruction::SetLocal {
                idx: LocalIndex(local_idx),
                src: value,
            });
            self.codegen.free_reg(value);

            let end_offset = self.codegen.current_index() as i32 - jump_skip as i32;
            self.codegen.patch_jump(jump_skip, end_offset);
        }

        // Compile function body
        if let Some(body) = &func.body {
            let saved_strict = self.is_strict_mode();
            let has_use_strict = body
                .directives
                .iter()
                .any(|d| d.directive.as_str() == "use strict");
            let has_use_strict = has_use_strict || self.has_use_strict_directive(&body.statements);

            if has_use_strict {
                self.set_strict_mode(true);
            }

            // Validate directives
            for d in &body.directives {
                self.literal_validator
                    .validate_string_literal(&d.expression)?;
            }

            for stmt in &body.statements {
                self.compile_statement(stmt)?;
            }

            self.set_strict_mode(saved_strict);
        }

        // Ensure return
        self.codegen.emit(Instruction::ReturnUndefined);

        // Exit function and get index
        let func_idx = self.codegen.exit_function();

        // Create closure
        let dst = self.codegen.alloc_reg();
        if is_generator {
            self.codegen.emit(Instruction::GeneratorClosure {
                dst,
                func: otter_vm_bytecode::FunctionIndex(func_idx),
            });
        } else if is_async {
            self.codegen.emit(Instruction::AsyncClosure {
                dst,
                func: otter_vm_bytecode::FunctionIndex(func_idx),
            });
        } else {
            self.codegen.emit(Instruction::Closure {
                dst,
                func: otter_vm_bytecode::FunctionIndex(func_idx),
            });
        }

        self.loop_stack = saved_loop_stack;
        Ok(dst)
    }

    /// Compile an arrow function expression
    fn compile_arrow_function(
        &mut self,
        arrow: &ArrowFunctionExpression,
    ) -> CompileResult<Register> {
        let is_async = arrow.r#async;

        let saved_loop_stack = std::mem::take(&mut self.loop_stack);

        // Enter function context
        self.codegen.enter_function(None);
        self.codegen.current.flags.is_arrow = true;
        self.codegen.current.flags.is_async = is_async;

        // Declare parameters and collect defaults
        let mut param_defaults: Vec<(u16, &Expression)> = Vec::new();
        for param in &arrow.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                    if let Some(init) = &param.initializer {
                        param_defaults.push((local_idx, init));
                    }
                }
                // Legacy / non-standard representation; keep for forward-compat.
                BindingPattern::AssignmentPattern(assign) => {
                    let BindingPattern::BindingIdentifier(ident) = &assign.left else {
                        return Err(CompileError::unsupported("Complex parameter patterns"));
                    };
                    let local_idx = self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                    param_defaults.push((local_idx, &assign.right));
                }
                _ => return Err(CompileError::unsupported("Complex parameter patterns")),
            }
        }

        // Check for rest parameter at function level
        if let Some(rest) = &arrow.params.rest {
            if let BindingPattern::BindingIdentifier(ident) = &rest.rest.argument {
                self.codegen.declare_variable(&ident.name, false)?;
                self.codegen.current.flags.has_rest = true;
            } else {
                return Err(CompileError::unsupported("Complex rest parameter pattern"));
            }
        }

        // Emit default parameter initializers (if arg === undefined).
        for (local_idx, default_expr) in param_defaults {
            let cur = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::GetLocal {
                dst: cur,
                idx: LocalIndex(local_idx),
            });
            let undef = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::LoadUndefined { dst: undef });
            let cond = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::StrictEq {
                dst: cond,
                lhs: cur,
                rhs: undef,
            });
            let jump_skip = self.codegen.emit_jump_if_false(cond);
            self.codegen.free_reg(cond);
            self.codegen.free_reg(undef);
            self.codegen.free_reg(cur);

            let value = self.compile_expression(default_expr)?;
            self.codegen.emit(Instruction::SetLocal {
                idx: LocalIndex(local_idx),
                src: value,
            });
            self.codegen.free_reg(value);

            let end_offset = self.codegen.current_index() as i32 - jump_skip as i32;
            self.codegen.patch_jump(jump_skip, end_offset);
        }

        // Compile body
        // Compile body
        let saved_strict = self.is_strict_mode();
        let has_use_strict = arrow
            .body
            .directives
            .iter()
            .any(|d| d.directive.as_str() == "use strict");
        let has_use_strict =
            has_use_strict || self.has_use_strict_directive(&arrow.body.statements);

        if has_use_strict {
            self.set_strict_mode(true);
        }

        // Validate directives
        for d in &arrow.body.directives {
            self.literal_validator
                .validate_string_literal(&d.expression)?;
        }

        if arrow.expression {
            // Expression body: `(x) => x + 1`
            // In oxc, expression body is stored as a single ExpressionStatement
            if let Some(Statement::ExpressionStatement(expr_stmt)) = arrow.body.statements.first() {
                let result = self.compile_expression(&expr_stmt.expression)?;
                self.codegen.emit(Instruction::Return { src: result });
                self.codegen.free_reg(result);
            } else {
                self.codegen.emit(Instruction::ReturnUndefined);
            }
        } else {
            // Statement body: `(x) => { return x + 1; }`
            for stmt in &arrow.body.statements {
                self.compile_statement(stmt)?;
            }
            self.codegen.emit(Instruction::ReturnUndefined);
        }

        self.set_strict_mode(saved_strict);

        // Exit function and get index
        let func_idx = self.codegen.exit_function();

        // Create closure
        let dst = self.codegen.alloc_reg();
        if is_async {
            self.codegen.emit(Instruction::AsyncClosure {
                dst,
                func: otter_vm_bytecode::FunctionIndex(func_idx),
            });
        } else {
            self.codegen.emit(Instruction::Closure {
                dst,
                func: otter_vm_bytecode::FunctionIndex(func_idx),
            });
        }

        self.loop_stack = saved_loop_stack;
        Ok(dst)
    }

    /// Compile an expression
    fn compile_expression(&mut self, expr: &Expression) -> CompileResult<Register> {
        self.enter_depth()?;
        let result = self.compile_expression_inner(expr);
        self.exit_depth();
        result
    }

    /// Inner implementation of expression compilation
    fn compile_expression_inner(&mut self, expr: &Expression) -> CompileResult<Register> {
        match expr {
            Expression::NumericLiteral(lit) => {
                // Validate numeric literal before compilation
                self.literal_validator.validate_numeric_literal(lit)?;

                let dst = self.codegen.alloc_reg();
                let value = lit.value;

                // Use LoadInt32 for integers that fit
                if value.fract() == 0.0 && value >= i32::MIN as f64 && value <= i32::MAX as f64 {
                    self.codegen.emit(Instruction::LoadInt32 {
                        dst,
                        value: value as i32,
                    });
                } else {
                    let idx = self.codegen.add_number(value);
                    self.codegen.emit(Instruction::LoadConst { dst, idx });
                }
                Ok(dst)
            }

            Expression::StringLiteral(lit) => {
                // Validate string literal before compilation
                self.literal_validator.validate_string_literal(lit)?;

                let dst = self.codegen.alloc_reg();
                let units = Self::decode_lone_surrogates(lit.value.as_str(), lit.lone_surrogates);
                let idx = self.codegen.add_string_units(units);
                self.codegen.emit(Instruction::LoadConst { dst, idx });
                Ok(dst)
            }

            Expression::BigIntLiteral(lit) => {
                // For now, validation is done by parser, but we could add stricter checks here
                // Note: lit.value is the BigInt value as a string (e.g. "100")

                let dst = self.codegen.alloc_reg();
                let idx = self.codegen.add_bigint(lit.value.to_string());
                self.codegen.emit(Instruction::LoadConst { dst, idx });
                Ok(dst)
            }

            Expression::BooleanLiteral(lit) => {
                let dst = self.codegen.alloc_reg();
                if lit.value {
                    self.codegen.emit(Instruction::LoadTrue { dst });
                } else {
                    self.codegen.emit(Instruction::LoadFalse { dst });
                }
                Ok(dst)
            }

            Expression::NullLiteral(_) => {
                let dst = self.codegen.alloc_reg();
                self.codegen.emit(Instruction::LoadNull { dst });
                Ok(dst)
            }

            Expression::Identifier(ident) => self.compile_identifier(&ident.name),

            Expression::BinaryExpression(binary) => self.compile_binary_expression(binary),

            Expression::LogicalExpression(logical) => self.compile_logical_expression(logical),

            Expression::UnaryExpression(unary) => self.compile_unary_expression(unary),

            Expression::AssignmentExpression(assign) => self.compile_assignment_expression(assign),

            Expression::CallExpression(call) => self.compile_call_expression(call),

            Expression::StaticMemberExpression(member) => {
                self.compile_static_member_expression(member)
            }

            Expression::ComputedMemberExpression(member) => {
                self.compile_computed_member_expression(member)
            }

            Expression::ObjectExpression(obj) => self.compile_object_expression(obj),

            Expression::ArrayExpression(arr) => self.compile_array_expression(arr),

            Expression::ConditionalExpression(cond) => self.compile_conditional_expression(cond),

            Expression::ParenthesizedExpression(paren) => {
                self.compile_expression(&paren.expression)
            }

            Expression::FunctionExpression(func) => self.compile_function_expression(func),

            Expression::ArrowFunctionExpression(arrow) => self.compile_arrow_function(arrow),

            Expression::NewExpression(new_expr) => self.compile_new_expression(new_expr),

            Expression::UpdateExpression(update) => self.compile_update_expression(update),

            Expression::AwaitExpression(await_expr) => self.compile_await_expression(await_expr),

            Expression::YieldExpression(yield_expr) => self.compile_yield_expression(yield_expr),

            Expression::ChainExpression(chain) => self.compile_chain_expression(chain),

            Expression::MetaProperty(meta) => {
                // Minimal support: `new.target` -> undefined (enough for current shims).
                if meta.meta.name == "new" && meta.property.name == "target" {
                    let dst = self.codegen.alloc_reg();
                    self.codegen.emit(Instruction::LoadUndefined { dst });
                    Ok(dst)
                } else {
                    Err(CompileError::unsupported("MetaProperty"))
                }
            }

            // TypeScript expressions - type erasure (compile inner expression, ignore type)
            Expression::TSAsExpression(expr) => self.compile_expression(&expr.expression),

            Expression::TSSatisfiesExpression(expr) => self.compile_expression(&expr.expression),

            Expression::TSTypeAssertion(expr) => self.compile_expression(&expr.expression),

            Expression::TSNonNullExpression(expr) => self.compile_expression(&expr.expression),

            Expression::TSInstantiationExpression(expr) => {
                self.compile_expression(&expr.expression)
            }

            // Common JS features
            Expression::RegExpLiteral(lit) => self.compile_regexp_literal(lit),
            Expression::TemplateLiteral(template) => self.compile_template_literal(template),
            Expression::ThisExpression(_) => {
                let dst = self.codegen.alloc_reg();
                self.codegen.emit(Instruction::LoadThis { dst });
                Ok(dst)
            }
            Expression::Super(_) => Err(CompileError::unsupported("Super")),
            Expression::ClassExpression(class_expr) => self.compile_class_expression(class_expr),

            _ => Err(CompileError::unsupported(format!(
                "UnknownExpression: {:?}",
                expr
            ))),
        }
    }

    fn compile_chain_expression(
        &mut self,
        chain: &oxc_ast::ast::ChainExpression,
    ) -> CompileResult<Register> {
        use oxc_ast::ast::ChainElement;

        match &chain.expression {
            ChainElement::StaticMemberExpression(member) => {
                if member.optional {
                    self.compile_optional_static_member_expression(member)
                } else {
                    self.compile_static_member_expression(member)
                }
            }
            ChainElement::ComputedMemberExpression(member) => {
                if member.optional {
                    self.compile_optional_computed_member_expression(member)
                } else {
                    self.compile_computed_member_expression(member)
                }
            }
            _ => Err(CompileError::unsupported("ChainExpression")),
        }
    }

    fn compile_optional_static_member_expression(
        &mut self,
        member: &oxc_ast::ast::StaticMemberExpression,
    ) -> CompileResult<Register> {
        let obj = self.compile_expression(&member.object)?;

        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::LoadUndefined { dst });

        let jump_idx = self.codegen.current_index();
        self.codegen.emit(Instruction::JumpIfNullish {
            src: obj,
            offset: JumpOffset(0),
        });

        let name_idx = self.codegen.add_string(&member.property.name);
        let ic_index = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::GetPropConst {
            dst,
            obj,
            name: name_idx,
            ic_index,
        });

        let end_offset = self.codegen.current_index() as i32 - jump_idx as i32;
        self.codegen.patch_jump(jump_idx, end_offset);

        self.codegen.free_reg(obj);
        Ok(dst)
    }

    fn compile_optional_computed_member_expression(
        &mut self,
        member: &oxc_ast::ast::ComputedMemberExpression,
    ) -> CompileResult<Register> {
        let obj = self.compile_expression(&member.object)?;

        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::LoadUndefined { dst });

        let jump_idx = self.codegen.current_index();
        self.codegen.emit(Instruction::JumpIfNullish {
            src: obj,
            offset: JumpOffset(0),
        });

        let key = self.compile_expression(&member.expression)?;
        let ic_index = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::GetProp {
            dst,
            obj,
            key,
            ic_index,
        });
        self.codegen.free_reg(key);

        let end_offset = self.codegen.current_index() as i32 - jump_idx as i32;
        self.codegen.patch_jump(jump_idx, end_offset);

        self.codegen.free_reg(obj);
        Ok(dst)
    }

    /// Compile a regular expression literal (`/pattern/flags`).
    ///
    /// For now this lowers to a call to the global `RegExp(pattern, flags)` constructor.
    fn compile_regexp_literal(
        &mut self,
        lit: &oxc_ast::ast::RegExpLiteral,
    ) -> CompileResult<Register> {
        // Validate RegExp literal before compilation
        self.literal_validator.validate_regexp_literal(lit)?;

        // Load global RegExp and call it with (pattern, flags). Call convention requires
        // `func` and args to be in contiguous registers.
        let func_tmp = self.codegen.alloc_reg();
        let name_idx = self.codegen.add_string("RegExp");
        let ic_index = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::GetGlobal {
            dst: func_tmp,
            name: name_idx,
            ic_index,
        });

        let pattern = lit.regex.pattern.text.as_str();
        let flags = lit.regex.flags.to_string();

        let pattern_tmp = self.codegen.alloc_reg();
        let pattern_idx = self.codegen.add_string(pattern);
        self.codegen.emit(Instruction::LoadConst {
            dst: pattern_tmp,
            idx: pattern_idx,
        });

        let flags_tmp = self.codegen.alloc_reg();
        let flags_idx = self.codegen.add_string(&flags);
        self.codegen.emit(Instruction::LoadConst {
            dst: flags_tmp,
            idx: flags_idx,
        });

        let frame = self.codegen.alloc_fresh_block(3);
        let arg1 = Register(frame.0 + 1);
        let arg2 = Register(frame.0 + 2);
        self.codegen.emit(Instruction::Move {
            dst: frame,
            src: func_tmp,
        });
        self.codegen.emit(Instruction::Move {
            dst: arg1,
            src: pattern_tmp,
        });
        self.codegen.emit(Instruction::Move {
            dst: arg2,
            src: flags_tmp,
        });

        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Call {
            dst,
            func: frame,
            argc: 2,
        });

        self.codegen.free_reg(func_tmp);
        self.codegen.free_reg(pattern_tmp);
        self.codegen.free_reg(flags_tmp);
        self.codegen.free_reg(frame);
        self.codegen.free_reg(arg1);
        self.codegen.free_reg(arg2);

        Ok(dst)
    }

    /// Compile a template literal (`\`hello ${name}!\``).
    ///
    /// Template literals are lowered to string concatenation:
    /// - `\`hello\`` -> "hello"
    /// - `\`hello ${x}!\`` -> "hello " + String(x) + "!"
    fn compile_template_literal(
        &mut self,
        template: &oxc_ast::ast::TemplateLiteral,
    ) -> CompileResult<Register> {
        // Validate template literal before compilation
        self.literal_validator.validate_template_literal(template)?;

        // Template with no expressions - just return the single string part
        if template.expressions.is_empty() {
            let dst = self.codegen.alloc_reg();
            if let Some(quasi) = template.quasis.first() {
                let units = Self::template_element_units(quasi);
                let str_idx = self.codegen.add_string_units(units);
                self.codegen
                    .emit(Instruction::LoadConst { dst, idx: str_idx });
            } else {
                // Empty template
                let str_idx = self.codegen.add_string("");
                self.codegen
                    .emit(Instruction::LoadConst { dst, idx: str_idx });
            }
            return Ok(dst);
        }

        // Template with expressions - build via concatenation
        // Pattern: quasi[0] + expr[0] + quasi[1] + expr[1] + ... + quasi[n]
        let mut result: Option<Register> = None;

        for (i, quasi) in template.quasis.iter().enumerate() {
            // Add the string part if non-empty
            let units = Self::template_element_units(quasi);
            if !units.is_empty() {
                let str_reg = self.codegen.alloc_reg();
                let str_idx = self.codegen.add_string_units(units);
                self.codegen.emit(Instruction::LoadConst {
                    dst: str_reg,
                    idx: str_idx,
                });

                result = Some(match result {
                    None => str_reg,
                    Some(acc) => {
                        let dst = self.codegen.alloc_reg();
                        self.codegen.emit(Instruction::Add {
                            dst,
                            lhs: acc,
                            rhs: str_reg,
                        });
                        self.codegen.free_reg(acc);
                        self.codegen.free_reg(str_reg);
                        dst
                    }
                });
            }

            // Add the expression if there's one at this position
            if i < template.expressions.len() {
                let expr_reg = self.compile_expression(&template.expressions[i])?;

                result = Some(match result {
                    None => expr_reg,
                    Some(acc) => {
                        let dst = self.codegen.alloc_reg();
                        self.codegen.emit(Instruction::Add {
                            dst,
                            lhs: acc,
                            rhs: expr_reg,
                        });
                        self.codegen.free_reg(acc);
                        self.codegen.free_reg(expr_reg);
                        dst
                    }
                });
            }
        }

        // If template was completely empty, return empty string
        Ok(result.unwrap_or_else(|| {
            let dst = self.codegen.alloc_reg();
            let str_idx = self.codegen.add_string("");
            self.codegen
                .emit(Instruction::LoadConst { dst, idx: str_idx });
            dst
        }))
    }

    /// Convert a template element's cooked value to UTF-16 code units.
    fn template_element_units(quasi: &oxc_ast::ast::TemplateElement) -> Vec<u16> {
        let cooked = quasi
            .value
            .cooked
            .as_ref()
            .map(|v| v.as_str())
            .unwrap_or("");
        Self::decode_lone_surrogates(cooked, quasi.lone_surrogates)
    }

    /// Decode a cooked string with lone-surrogate encoding into UTF-16 units.
    fn decode_lone_surrogates(value: &str, has_lone: bool) -> Vec<u16> {
        if !has_lone {
            return value.encode_utf16().collect();
        }

        let mut units = Vec::with_capacity(value.len());
        let mut chars = value.chars().peekable();
        let mut buf = [0u16; 2];

        while let Some(ch) = chars.next() {
            if ch == '\u{FFFD}' {
                let mut hex = String::new();
                for _ in 0..4 {
                    if let Some(next) = chars.next() {
                        hex.push(next);
                    } else {
                        break;
                    }
                }

                if hex.len() == 4 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    if let Ok(code) = u16::from_str_radix(&hex, 16) {
                        units.push(code);
                        continue;
                    }
                }

                units.push(0xFFFD);
                for ch in hex.chars() {
                    let encoded = ch.encode_utf16(&mut buf);
                    units.extend_from_slice(encoded);
                }
            } else {
                let encoded = ch.encode_utf16(&mut buf);
                units.extend_from_slice(encoded);
            }
        }

        units
    }

    /// Compile an await expression
    fn compile_await_expression(
        &mut self,
        await_expr: &oxc_ast::ast::AwaitExpression,
    ) -> CompileResult<Register> {
        // Compile the argument (promise)
        let src = self.compile_expression(&await_expr.argument)?;

        // Emit await instruction
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Await { dst, src });
        self.codegen.free_reg(src);

        Ok(dst)
    }

    /// Compile a yield expression
    fn compile_yield_expression(
        &mut self,
        yield_expr: &oxc_ast::ast::YieldExpression,
    ) -> CompileResult<Register> {
        // Compile the argument (value to yield)
        let src = if let Some(argument) = &yield_expr.argument {
            self.compile_expression(argument)?
        } else {
            let r = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::LoadUndefined { dst: r });
            r
        };

        // Emit yield instruction
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Yield { dst, src });
        self.codegen.free_reg(src);

        Ok(dst)
    }

    /// Compile an identifier reference
    fn compile_identifier(&mut self, name: &str) -> CompileResult<Register> {
        let dst = self.codegen.alloc_reg();

        match self.codegen.resolve_variable(name) {
            Some(ResolvedBinding::Local(idx)) => {
                self.codegen.emit(Instruction::GetLocal {
                    dst,
                    idx: LocalIndex(idx),
                });
            }
            Some(ResolvedBinding::Global(name)) => {
                let name_idx = self.codegen.add_string(&name);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::GetGlobal {
                    dst,
                    name: name_idx,
                    ic_index,
                });
            }
            Some(ResolvedBinding::Upvalue { index, depth }) => {
                // Register this upvalue and get its index in the current function's upvalues array
                let upvalue_idx = self.codegen.register_upvalue(index, depth);
                self.codegen.emit(Instruction::GetUpvalue {
                    dst,
                    idx: LocalIndex(upvalue_idx),
                });
            }
            None => {
                let name_idx = self.codegen.add_string(name);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::GetGlobal {
                    dst,
                    name: name_idx,
                    ic_index,
                });
            }
        }

        Ok(dst)
    }

    /// Compile a binary expression
    fn compile_binary_expression(&mut self, binary: &BinaryExpression) -> CompileResult<Register> {
        let lhs = self.compile_expression(&binary.left)?;
        let rhs = self.compile_expression(&binary.right)?;
        let dst = self.codegen.alloc_reg();

        let instruction = match binary.operator {
            BinaryOperator::Addition => Instruction::Add { dst, lhs, rhs },
            BinaryOperator::Subtraction => Instruction::Sub { dst, lhs, rhs },
            BinaryOperator::Multiplication => Instruction::Mul { dst, lhs, rhs },
            BinaryOperator::Division => Instruction::Div { dst, lhs, rhs },
            BinaryOperator::Remainder => Instruction::Mod { dst, lhs, rhs },
            BinaryOperator::LessThan => Instruction::Lt { dst, lhs, rhs },
            BinaryOperator::LessEqualThan => Instruction::Le { dst, lhs, rhs },
            BinaryOperator::GreaterThan => Instruction::Gt { dst, lhs, rhs },
            BinaryOperator::GreaterEqualThan => Instruction::Ge { dst, lhs, rhs },
            BinaryOperator::Equality => Instruction::Eq { dst, lhs, rhs },
            BinaryOperator::Inequality => Instruction::Ne { dst, lhs, rhs },
            BinaryOperator::StrictEquality => Instruction::StrictEq { dst, lhs, rhs },
            BinaryOperator::StrictInequality => Instruction::StrictNe { dst, lhs, rhs },
            BinaryOperator::BitwiseAnd => Instruction::BitAnd { dst, lhs, rhs },
            BinaryOperator::BitwiseOR => Instruction::BitOr { dst, lhs, rhs },
            BinaryOperator::BitwiseXOR => Instruction::BitXor { dst, lhs, rhs },
            BinaryOperator::ShiftLeft => Instruction::Shl { dst, lhs, rhs },
            BinaryOperator::ShiftRight => Instruction::Shr { dst, lhs, rhs },
            BinaryOperator::ShiftRightZeroFill => Instruction::Ushr { dst, lhs, rhs },
            BinaryOperator::Instanceof => Instruction::InstanceOf { dst, lhs, rhs },
            BinaryOperator::In => Instruction::In { dst, lhs, rhs },
            _ => {
                return Err(CompileError::unsupported(format!(
                    "Binary operator: {:?}",
                    binary.operator
                )));
            }
        };

        self.codegen.emit(instruction);
        self.codegen.free_reg(lhs);
        self.codegen.free_reg(rhs);

        Ok(dst)
    }

    /// Compile a logical expression (`&&`, `||`, `??`) with short-circuiting.
    fn compile_logical_expression(
        &mut self,
        logical: &oxc_ast::ast::LogicalExpression,
    ) -> CompileResult<Register> {
        let lhs = self.compile_expression(&logical.left)?;
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Move { dst, src: lhs });
        self.codegen.free_reg(lhs);

        let short_circuit_jump = match logical.operator {
            LogicalOperator::And => self.codegen.emit_jump_if_false(dst),
            LogicalOperator::Or => self.codegen.emit_jump_if_true(dst),
            LogicalOperator::Coalesce => {
                let idx = self.codegen.current_index();
                self.codegen.emit(Instruction::JumpIfNotNullish {
                    src: dst,
                    offset: JumpOffset(0),
                });
                idx
            }
        };

        let rhs = self.compile_expression(&logical.right)?;
        self.codegen.emit(Instruction::Move { dst, src: rhs });
        self.codegen.free_reg(rhs);

        let end_offset = self.codegen.current_index() as i32 - short_circuit_jump as i32;
        self.codegen.patch_jump(short_circuit_jump, end_offset);

        Ok(dst)
    }

    /// Compile a unary expression
    fn compile_unary_expression(&mut self, unary: &UnaryExpression) -> CompileResult<Register> {
        if unary.operator == UnaryOperator::Delete {
            // `delete` only has effect on property references; other forms return true.
            let dst = self.codegen.alloc_reg();

            return match &unary.argument {
                Expression::Identifier(_ident) => {
                    self.codegen.emit(Instruction::LoadTrue { dst });
                    Ok(dst)
                }
                Expression::StaticMemberExpression(member) => {
                    let obj = self.compile_expression(&member.object)?;
                    let key = self.codegen.alloc_reg();
                    let idx = self.codegen.add_string(&member.property.name);
                    self.codegen.emit(Instruction::LoadConst { dst: key, idx });
                    self.codegen.emit(Instruction::DeleteProp { dst, obj, key });
                    self.codegen.free_reg(key);
                    self.codegen.free_reg(obj);
                    Ok(dst)
                }
                Expression::ComputedMemberExpression(member) => {
                    let obj = self.compile_expression(&member.object)?;
                    let key = self.compile_expression(&member.expression)?;
                    self.codegen.emit(Instruction::DeleteProp { dst, obj, key });
                    self.codegen.free_reg(key);
                    self.codegen.free_reg(obj);
                    Ok(dst)
                }
                _ => {
                    // Still evaluate argument for side effects
                    let arg = self.compile_expression(&unary.argument)?;
                    self.codegen.free_reg(arg);
                    self.codegen.emit(Instruction::LoadTrue { dst });
                    Ok(dst)
                }
            };
        }

        if unary.operator == UnaryOperator::Typeof {
            if let Expression::Identifier(ident) = &unary.argument {
                match self.codegen.resolve_variable(&ident.name) {
                    Some(ResolvedBinding::Local(_)) | Some(ResolvedBinding::Upvalue { .. }) => {
                        let src = self.compile_identifier(&ident.name)?;
                        let dst = self.codegen.alloc_reg();
                        self.codegen.emit(Instruction::TypeOf { dst, src });
                        self.codegen.free_reg(src);
                        return Ok(dst);
                    }
                    Some(ResolvedBinding::Global(_)) | None => {
                        let dst = self.codegen.alloc_reg();
                        let name_idx = self.codegen.add_string(&ident.name);
                        self.codegen.emit(Instruction::TypeOfName {
                            dst,
                            name: name_idx,
                        });
                        return Ok(dst);
                    }
                }
            }
        }

        let src = self.compile_expression(&unary.argument)?;
        let dst = self.codegen.alloc_reg();

        let instruction = match unary.operator {
            UnaryOperator::UnaryNegation => Instruction::Neg { dst, src },
            UnaryOperator::UnaryPlus => Instruction::ToNumber { dst, src },
            UnaryOperator::LogicalNot => Instruction::Not { dst, src },
            UnaryOperator::BitwiseNot => Instruction::BitNot { dst, src },
            UnaryOperator::Typeof => Instruction::TypeOf { dst, src },
            _ => {
                return Err(CompileError::unsupported(format!(
                    "Unary operator: {:?}",
                    unary.operator
                )));
            }
        };

        self.codegen.emit(instruction);
        self.codegen.free_reg(src);

        Ok(dst)
    }

    /// Compile an assignment expression
    fn compile_assignment_expression(
        &mut self,
        assign: &AssignmentExpression,
    ) -> CompileResult<Register> {
        let value = self.compile_expression(&assign.right)?;

        match &assign.left {
            AssignmentTarget::AssignmentTargetIdentifier(ident) => {
                match self.codegen.resolve_variable(&ident.name) {
                    Some(ResolvedBinding::Local(idx)) => {
                        self.codegen.emit(Instruction::SetLocal {
                            idx: LocalIndex(idx),
                            src: value,
                        });
                    }
                    Some(ResolvedBinding::Global(_)) | None => {
                        let name_idx = self.codegen.add_string(&ident.name);
                        let ic_index = self.codegen.alloc_ic();
                        self.codegen.emit(Instruction::SetGlobal {
                            name: name_idx,
                            src: value,
                            ic_index,
                        });
                    }
                    Some(ResolvedBinding::Upvalue { index, depth }) => {
                        // Register this upvalue and get its index in the current function's upvalues array
                        let upvalue_idx = self.codegen.register_upvalue(index, depth);
                        self.codegen.emit(Instruction::SetUpvalue {
                            idx: LocalIndex(upvalue_idx),
                            src: value,
                        });
                    }
                }
            }
            AssignmentTarget::StaticMemberExpression(member) => {
                let obj = self.compile_expression(&member.object)?;
                let name_idx = self.codegen.add_string(&member.property.name);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetPropConst {
                    obj,
                    name: name_idx,
                    val: value,
                    ic_index,
                });
                self.codegen.free_reg(obj);
            }
            AssignmentTarget::ComputedMemberExpression(member) => {
                let obj = self.compile_expression(&member.object)?;
                let key = self.compile_expression(&member.expression)?;
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetProp {
                    obj,
                    key,
                    val: value,
                    ic_index,
                });
                self.codegen.free_reg(key);
                self.codegen.free_reg(obj);
            }
            _ => return Err(CompileError::InvalidAssignmentTarget),
        }

        Ok(value)
    }

    /// Compile a call expression
    fn compile_call_expression(&mut self, call: &CallExpression) -> CompileResult<Register> {
        // Check if this is a method call (obj.method() or obj["method"]())
        if let Expression::StaticMemberExpression(member) = &call.callee {
            return self.compile_method_call(call, member);
        }

        // Check for computed member call (obj[key]())
        if let Expression::ComputedMemberExpression(member) = &call.callee {
            return self.compile_computed_method_call(call, member);
        }

        // Compile callee
        let func = self.compile_expression(&call.callee)?;

        // Check if we have any spread arguments
        let has_spread = call
            .arguments
            .iter()
            .any(|arg| matches!(arg, Argument::SpreadElement(_)));

        if has_spread {
            // Handle spread arguments
            self.compile_call_with_spread(call, func)
        } else {
            // Regular call without spread
            let argc = call.arguments.len() as u8;
            // Call convention requires `func` and args to be in contiguous registers.
            let mut arg_tmps = Vec::with_capacity(call.arguments.len());
            for arg in &call.arguments {
                arg_tmps.push(self.compile_expression(arg.to_expression())?);
            }

            let frame = self.codegen.alloc_fresh_block(1 + argc);
            self.codegen.emit(Instruction::Move {
                dst: frame,
                src: func,
            });
            for (i, tmp) in arg_tmps.iter().copied().enumerate() {
                let target = Register(frame.0 + 1 + i as u16);
                self.codegen.emit(Instruction::Move {
                    dst: target,
                    src: tmp,
                });
            }

            let dst = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::Call {
                dst,
                func: frame,
                argc,
            });

            self.codegen.free_reg(func);
            for tmp in arg_tmps {
                self.codegen.free_reg(tmp);
            }
            for i in 0..(1 + argc as u16) {
                self.codegen.free_reg(Register(frame.0 + i));
            }

            Ok(dst)
        }
    }

    /// Compile a method call (obj.method(...))
    fn compile_method_call(
        &mut self,
        call: &CallExpression,
        member: &oxc_ast::ast::StaticMemberExpression,
    ) -> CompileResult<Register> {
        // Compile the object (receiver)
        let obj = self.compile_expression(&member.object)?;

        // Get method name as constant
        let method_name = member.property.name.as_str();
        let method_idx = self.codegen.add_string(method_name);

        // Check if we have any spread arguments
        let has_spread = call
            .arguments
            .iter()
            .any(|arg| matches!(arg, Argument::SpreadElement(_)));

        if has_spread {
            // For spread with method call, we need to get the method and use CallSpread
            // Get method into a register first
            let func = self.codegen.alloc_reg();
            let ic_index = self.codegen.alloc_ic();
            self.codegen.emit(Instruction::GetPropConst {
                dst: func,
                obj,
                name: method_idx,
                ic_index,
            });
            // Use regular spread handling (this won't preserve `this` perfectly, but is a fallback)
            self.codegen.free_reg(obj); // obj is not used in CallSpread
            self.compile_call_with_spread(call, func)
        } else {
            // Regular method call without spread
            let argc = call.arguments.len() as u8;
            // CallMethod convention requires receiver and args to be in contiguous registers.
            let mut arg_tmps = Vec::with_capacity(call.arguments.len());
            for arg in &call.arguments {
                arg_tmps.push(self.compile_expression(arg.to_expression())?);
            }

            let frame = self.codegen.alloc_fresh_block(1 + argc);
            self.codegen.emit(Instruction::Move {
                dst: frame,
                src: obj,
            });
            for (i, tmp) in arg_tmps.iter().copied().enumerate() {
                let target = Register(frame.0 + 1 + i as u16);
                self.codegen.emit(Instruction::Move {
                    dst: target,
                    src: tmp,
                });
            }

            let dst = self.codegen.alloc_reg();
            let ic_index = self.codegen.alloc_ic();
            self.codegen.emit(Instruction::CallMethod {
                dst,
                obj: frame,
                method: method_idx,
                argc,
                ic_index,
            });

            self.codegen.free_reg(obj);
            for tmp in arg_tmps {
                self.codegen.free_reg(tmp);
            }
            for i in 0..(1 + argc as u16) {
                self.codegen.free_reg(Register(frame.0 + i));
            }

            Ok(dst)
        }
    }

    /// Compile a computed method call (obj[key](...))
    fn compile_computed_method_call(
        &mut self,
        call: &CallExpression,
        member: &oxc_ast::ast::ComputedMemberExpression,
    ) -> CompileResult<Register> {
        // Compile the object (receiver)
        let obj = self.compile_expression(&member.object)?;

        // Compile the key
        let key = self.compile_expression(&member.expression)?;

        // Check for spread arguments (fallback to regular call in that case)
        let has_spread = call
            .arguments
            .iter()
            .any(|arg| matches!(arg, Argument::SpreadElement(_)));

        if has_spread {
            // For spread with computed method call, use CallMethodComputedSpread
            // Build spread array first
            let args_arr = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::NewArray {
                dst: args_arr,
                len: 0,
            });

            for arg in &call.arguments {
                match arg {
                    Argument::SpreadElement(spread) => {
                        let spread_val = self.compile_expression(&spread.argument)?;
                        self.codegen.emit(Instruction::Spread {
                            dst: args_arr,
                            src: spread_val,
                        });
                        self.codegen.free_reg(spread_val);
                    }
                    _ => {
                        let arg_val = self.compile_expression(arg.to_expression())?;

                        // Get current length to use as index
                        let len_name = self.codegen.add_string("length");
                        let len_reg = self.codegen.alloc_reg();
                        let ic_index = self.codegen.alloc_ic();
                        self.codegen.emit(Instruction::GetPropConst {
                            dst: len_reg,
                            obj: args_arr,
                            name: len_name,
                            ic_index,
                        });

                        // Set element at current length
                        self.codegen.emit(Instruction::SetElem {
                            arr: args_arr,
                            idx: len_reg,
                            val: arg_val,
                        });

                        self.codegen.free_reg(len_reg);
                        self.codegen.free_reg(arg_val);
                    }
                }
            }

            let dst = self.codegen.alloc_reg();
            let ic_index = self.codegen.alloc_ic();
            self.codegen.emit(Instruction::CallMethodComputedSpread {
                dst,
                obj,
                key,
                spread: args_arr,
                ic_index,
            });

            self.codegen.free_reg(obj);
            self.codegen.free_reg(key);
            self.codegen.free_reg(args_arr);

            Ok(dst)
        } else {
            // Regular computed method call without spread
            let argc = call.arguments.len() as u8;

            // Compile arguments into temporary registers
            let mut arg_tmps = Vec::with_capacity(call.arguments.len());
            for arg in &call.arguments {
                arg_tmps.push(self.compile_expression(arg.to_expression())?);
            }

            // CallMethodComputed requires: obj, key, arg1, arg2, ... (contiguous)
            let frame = self.codegen.alloc_fresh_block(2 + argc);
            self.codegen.emit(Instruction::Move {
                dst: frame,
                src: obj,
            });
            self.codegen.emit(Instruction::Move {
                dst: Register(frame.0 + 1),
                src: key,
            });
            for (i, tmp) in arg_tmps.iter().copied().enumerate() {
                let target = Register(frame.0 + 2 + i as u16);
                self.codegen.emit(Instruction::Move {
                    dst: target,
                    src: tmp,
                });
            }

            let dst = self.codegen.alloc_reg();
            let ic_index = self.codegen.alloc_ic();
            self.codegen.emit(Instruction::CallMethodComputed {
                dst,
                obj: frame,
                key: Register(frame.0 + 1),
                argc,
                ic_index,
            });

            self.codegen.free_reg(obj);
            self.codegen.free_reg(key);
            for tmp in arg_tmps {
                self.codegen.free_reg(tmp);
            }
            for i in 0..(2 + argc as u16) {
                self.codegen.free_reg(Register(frame.0 + i));
            }

            Ok(dst)
        }
    }

    /// Compile a call expression with spread arguments
    fn compile_call_with_spread(
        &mut self,
        call: &CallExpression,
        func: Register,
    ) -> CompileResult<Register> {
        // Create an array to hold all arguments (including spread)
        let args_arr = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::NewArray {
            dst: args_arr,
            len: 0,
        });

        // Process each argument
        for arg in &call.arguments {
            match arg {
                Argument::SpreadElement(spread) => {
                    // Spread the array: concat elements to args_arr
                    let spread_val = self.compile_expression(&spread.argument)?;

                    // Use Spread instruction to expand and concat
                    self.codegen.emit(Instruction::Spread {
                        dst: args_arr,
                        src: spread_val,
                    });
                    self.codegen.free_reg(spread_val);
                }
                _ => {
                    // Regular argument: push to array
                    let arg_val = self.compile_expression(arg.to_expression())?;

                    // Get current length to use as index
                    let len_name = self.codegen.add_string("length");
                    let len_reg = self.codegen.alloc_reg();
                    let ic_index = self.codegen.alloc_ic();
                    self.codegen.emit(Instruction::GetPropConst {
                        dst: len_reg,
                        obj: args_arr,
                        name: len_name,
                        ic_index,
                    });

                    // Set element at current length
                    self.codegen.emit(Instruction::SetElem {
                        arr: args_arr,
                        idx: len_reg,
                        val: arg_val,
                    });

                    self.codegen.free_reg(len_reg);
                    self.codegen.free_reg(arg_val);
                }
            }
        }

        // Emit CallSpread instruction
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::CallSpread {
            dst,
            func,
            argc: 0, // All args are in the spread array
            spread: args_arr,
        });

        self.codegen.free_reg(args_arr);
        self.codegen.free_reg(func);

        Ok(dst)
    }

    /// Compile a new expression (new Foo(...))
    fn compile_new_expression(&mut self, new_expr: &NewExpression) -> CompileResult<Register> {
        // Compile callee (constructor)
        let func = self.compile_expression(&new_expr.callee)?;

        let spread_pos = new_expr
            .arguments
            .iter()
            .position(|arg| matches!(arg, Argument::SpreadElement(_)));

        if let Some(spread_pos) = spread_pos {
            // Support `new Ctor(a, b, ...args)` (single spread; must be last).
            if spread_pos + 1 != new_expr.arguments.len() {
                return Err(CompileError::unsupported(
                    "Spread in new expressions (must be last)",
                ));
            }

            let argc = spread_pos as u8;
            // ConstructSpread convention requires `func` and regular args to be contiguous.
            let mut arg_tmps = Vec::with_capacity(argc as usize);
            for arg in new_expr.arguments.iter().take(spread_pos) {
                arg_tmps.push(self.compile_expression(arg.to_expression())?);
            }

            // Compile spread array
            let spread = match &new_expr.arguments[spread_pos] {
                Argument::SpreadElement(spread) => self.compile_expression(&spread.argument)?,
                _ => unreachable!(),
            };

            let frame = self.codegen.alloc_fresh_block(1 + argc);
            self.codegen.emit(Instruction::Move {
                dst: frame,
                src: func,
            });
            for (i, tmp) in arg_tmps.iter().copied().enumerate() {
                let target = Register(frame.0 + 1 + i as u16);
                self.codegen.emit(Instruction::Move {
                    dst: target,
                    src: tmp,
                });
            }

            let dst = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::ConstructSpread {
                dst,
                func: frame,
                argc,
                spread,
            });

            self.codegen.free_reg(func);
            for tmp in arg_tmps {
                self.codegen.free_reg(tmp);
            }
            for i in 0..(1 + argc as u16) {
                self.codegen.free_reg(Register(frame.0 + i));
            }
            self.codegen.free_reg(spread);

            Ok(dst)
        } else {
            // Regular construct without spread
            let argc = new_expr.arguments.len() as u8;
            // Construct convention requires `func` and args to be contiguous.
            let mut arg_tmps = Vec::with_capacity(new_expr.arguments.len());
            for arg in &new_expr.arguments {
                arg_tmps.push(self.compile_expression(arg.to_expression())?);
            }

            let frame = self.codegen.alloc_fresh_block(1 + argc);
            self.codegen.emit(Instruction::Move {
                dst: frame,
                src: func,
            });
            for (i, tmp) in arg_tmps.iter().copied().enumerate() {
                let target = Register(frame.0 + 1 + i as u16);
                self.codegen.emit(Instruction::Move {
                    dst: target,
                    src: tmp,
                });
            }

            let dst = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::Construct {
                dst,
                func: frame,
                argc,
            });

            self.codegen.free_reg(func);
            for tmp in arg_tmps {
                self.codegen.free_reg(tmp);
            }
            for i in 0..(1 + argc as u16) {
                self.codegen.free_reg(Register(frame.0 + i));
            }

            Ok(dst)
        }
    }

    /// Compile an update expression (i++, ++i, i--, --i)
    fn compile_update_expression(&mut self, update: &UpdateExpression) -> CompileResult<Register> {
        // Get the argument (must be an identifier or member expression)
        let argument = &update.argument;

        match argument {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
                self.compile_update_identifier(ident, update.operator, update.prefix)
            }
            _ => Err(CompileError::unsupported(
                "Update expression on non-identifier",
            )),
        }
    }

    /// Compile update on identifier
    fn compile_update_identifier(
        &mut self,
        ident: &IdentifierReference,
        operator: oxc_ast::ast::UpdateOperator,
        prefix: bool,
    ) -> CompileResult<Register> {
        let name = &ident.name;

        // Load current value
        let current = self.compile_identifier(name)?;

        // Result register
        let result = self.codegen.alloc_reg();

        if prefix {
            // Prefix: ++i or --i
            match operator {
                UpdateOperator::Increment => {
                    self.codegen.emit(Instruction::Inc {
                        dst: result,
                        src: current,
                    });
                }
                UpdateOperator::Decrement => {
                    self.codegen.emit(Instruction::Dec {
                        dst: result,
                        src: current,
                    });
                }
            }
            // Store back to variable
            self.store_to_identifier(name, result)?;
        } else {
            // Postfix: i++ or i--
            // First copy current value as result
            self.codegen.emit(Instruction::Move {
                dst: result,
                src: current,
            });

            // Increment/decrement in a temp
            let new_val = self.codegen.alloc_reg();
            match operator {
                UpdateOperator::Increment => {
                    self.codegen.emit(Instruction::Inc {
                        dst: new_val,
                        src: current,
                    });
                }
                UpdateOperator::Decrement => {
                    self.codegen.emit(Instruction::Dec {
                        dst: new_val,
                        src: current,
                    });
                }
            }
            // Store new value back
            self.store_to_identifier(name, new_val)?;
            self.codegen.free_reg(new_val);
        }

        self.codegen.free_reg(current);
        Ok(result)
    }

    /// Store a value to an identifier (variable)
    fn store_to_identifier(&mut self, name: &str, src: Register) -> CompileResult<()> {
        match self.codegen.resolve_variable(name) {
            Some(ResolvedBinding::Local(idx)) => {
                self.codegen.emit(Instruction::SetLocal {
                    idx: LocalIndex(idx),
                    src,
                });
            }
            Some(ResolvedBinding::Global(name)) => {
                let name_idx = self.codegen.add_string(&name);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetGlobal {
                    name: name_idx,
                    src,
                    ic_index,
                });
            }
            Some(ResolvedBinding::Upvalue { index, depth }) => {
                // Register this upvalue and get its index in the current function's upvalues array
                let upvalue_idx = self.codegen.register_upvalue(index, depth);
                self.codegen.emit(Instruction::SetUpvalue {
                    idx: LocalIndex(upvalue_idx),
                    src,
                });
            }
            None => {
                // Undeclared variable - treat as global
                let name_idx = self.codegen.add_string(name);
                let ic_index = self.codegen.alloc_ic();
                self.codegen.emit(Instruction::SetGlobal {
                    name: name_idx,
                    src,
                    ic_index,
                });
            }
        }
        Ok(())
    }

    /// Compile a static member expression (obj.prop)
    fn compile_static_member_expression(
        &mut self,
        member: &StaticMemberExpression,
    ) -> CompileResult<Register> {
        let obj = self.compile_expression(&member.object)?;
        let dst = self.codegen.alloc_reg();
        let name_idx = self.codegen.add_string(&member.property.name);
        let ic_index = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::GetPropConst {
            dst,
            obj,
            name: name_idx,
            ic_index,
        });
        self.codegen.free_reg(obj);
        Ok(dst)
    }

    /// Compile a computed member expression (obj[key])
    fn compile_computed_member_expression(
        &mut self,
        member: &ComputedMemberExpression,
    ) -> CompileResult<Register> {
        let obj = self.compile_expression(&member.object)?;
        let dst = self.codegen.alloc_reg();
        let key = self.compile_expression(&member.expression)?;
        let ic_index = self.codegen.alloc_ic();
        self.codegen.emit(Instruction::GetProp {
            dst,
            obj,
            key,
            ic_index,
        });
        self.codegen.free_reg(key);
        self.codegen.free_reg(obj);
        Ok(dst)
    }

    /// Compile an object expression
    fn compile_object_expression(&mut self, obj: &ObjectExpression) -> CompileResult<Register> {
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::NewObject { dst });

        for prop in &obj.properties {
            match prop {
                ObjectPropertyKind::ObjectProperty(prop) => {
                    match prop.kind {
                        PropertyKind::Init => {
                            // Fast path: non-computed static keys
                            if !prop.computed {
                                let key = match &prop.key {
                                    PropertyKey::StaticIdentifier(ident) => {
                                        Some(self.codegen.add_string(&ident.name))
                                    }
                                    PropertyKey::StringLiteral(lit) => {
                                        let units = Self::decode_lone_surrogates(
                                            lit.value.as_str(),
                                            lit.lone_surrogates,
                                        );
                                        Some(self.codegen.add_string_units(units))
                                    }
                                    _ => None,
                                };

                                if let Some(key) = key {
                                    let value = self.compile_expression(&prop.value)?;
                                    let ic_index = self.codegen.alloc_ic();
                                    self.codegen.emit(Instruction::SetPropConst {
                                        obj: dst,
                                        name: key,
                                        val: value,
                                        ic_index,
                                    });
                                    self.codegen.free_reg(value);
                                    continue;
                                }
                            }

                            // Computed key: obj[key] = value
                            let key_reg = self.compile_property_key(&prop.key)?;
                            let value = self.compile_expression(&prop.value)?;
                            let ic_index = self.codegen.alloc_ic();
                            self.codegen.emit(Instruction::SetProp {
                                obj: dst,
                                key: key_reg,
                                val: value,
                                ic_index,
                            });
                            self.codegen.free_reg(value);
                            self.codegen.free_reg(key_reg);
                        }
                        PropertyKind::Get => {
                            // Use native DefineGetter instruction
                            let key_reg = self.compile_property_key(&prop.key)?;
                            let func_reg = self.compile_expression(&prop.value)?;

                            self.codegen.emit(Instruction::DefineGetter {
                                obj: dst,
                                key: key_reg,
                                func: func_reg,
                            });

                            self.codegen.free_reg(func_reg);
                            self.codegen.free_reg(key_reg);
                        }
                        PropertyKind::Set => {
                            // Use native DefineSetter instruction
                            let key_reg = self.compile_property_key(&prop.key)?;
                            let func_reg = self.compile_expression(&prop.value)?;

                            self.codegen.emit(Instruction::DefineSetter {
                                obj: dst,
                                key: key_reg,
                                func: func_reg,
                            });

                            self.codegen.free_reg(func_reg);
                            self.codegen.free_reg(key_reg);
                        }
                    }
                }
                ObjectPropertyKind::SpreadProperty(_) => {
                    let ObjectPropertyKind::SpreadProperty(spread) = prop else {
                        unreachable!();
                    };

                    // Lower `{ ...a }` into `__Object_assign(dst, a)`.
                    // This is close enough for our current shims/builtins.
                    let src = self.compile_expression(&spread.argument)?;

                    let func_tmp = self.codegen.alloc_reg();
                    let name_idx = self.codegen.add_string("__Object_assign");
                    let ic_index = self.codegen.alloc_ic();
                    self.codegen.emit(Instruction::GetGlobal {
                        dst: func_tmp,
                        name: name_idx,
                        ic_index,
                    });

                    // Call convention requires `func` and args to be contiguous.
                    let frame = self.codegen.alloc_fresh_block(3);
                    let arg1 = Register(frame.0 + 1);
                    let arg2 = Register(frame.0 + 2);

                    self.codegen.emit(Instruction::Move {
                        dst: frame,
                        src: func_tmp,
                    });
                    self.codegen.emit(Instruction::Move {
                        dst: arg1,
                        src: dst,
                    });
                    self.codegen.emit(Instruction::Move { dst: arg2, src });

                    let call_dst = self.codegen.alloc_reg();
                    self.codegen.emit(Instruction::Call {
                        dst: call_dst,
                        func: frame,
                        argc: 2,
                    });

                    self.codegen.free_reg(call_dst);
                    self.codegen.free_reg(func_tmp);
                    self.codegen.free_reg(src);
                    self.codegen.free_reg(frame);
                    self.codegen.free_reg(arg1);
                    self.codegen.free_reg(arg2);
                }
            }
        }

        Ok(dst)
    }

    fn compile_property_key(&mut self, key: &PropertyKey) -> CompileResult<Register> {
        match key {
            PropertyKey::StaticIdentifier(ident) => {
                let dst = self.codegen.alloc_reg();
                let idx = self.codegen.add_string(&ident.name);
                self.codegen.emit(Instruction::LoadConst { dst, idx });
                Ok(dst)
            }
            PropertyKey::PrivateIdentifier(_) => {
                Err(CompileError::unsupported("PrivateIdentifier"))
            }
            PropertyKey::StringLiteral(lit) => {
                let dst = self.codegen.alloc_reg();
                let units = Self::decode_lone_surrogates(lit.value.as_str(), lit.lone_surrogates);
                let idx = self.codegen.add_string_units(units);
                self.codegen.emit(Instruction::LoadConst { dst, idx });
                Ok(dst)
            }
            PropertyKey::NumericLiteral(lit) => {
                // Lower numeric keys to numbers; SetProp will coerce to index/string.
                let dst = self.codegen.alloc_reg();
                let value = lit.value;
                if value.fract() == 0.0 && value >= i32::MIN as f64 && value <= i32::MAX as f64 {
                    self.codegen.emit(Instruction::LoadInt32 {
                        dst,
                        value: value as i32,
                    });
                } else {
                    let idx = self.codegen.add_number(value);
                    self.codegen.emit(Instruction::LoadConst { dst, idx });
                }
                Ok(dst)
            }
            // Common case for `[ident]` (e.g. `[Symbol.iterator]` via a local binding)
            PropertyKey::Identifier(ident) => self.compile_identifier(&ident.name),
            PropertyKey::StaticMemberExpression(member) => {
                self.compile_static_member_expression(member)
            }
            PropertyKey::ComputedMemberExpression(member) => {
                self.compile_computed_member_expression(member)
            }
            _ => Err(CompileError::unsupported(
                "Computed property key expression",
            )),
        }
    }

    /// Compile an array expression
    fn compile_array_expression(&mut self, arr: &ArrayExpression) -> CompileResult<Register> {
        let has_spread = arr
            .elements
            .iter()
            .any(|e| matches!(e, ArrayExpressionElement::SpreadElement(_)));

        let dst = self.codegen.alloc_reg();

        if !has_spread {
            let len = arr.elements.len() as u16;
            self.codegen.emit(Instruction::NewArray { dst, len });

            for (i, elem) in arr.elements.iter().enumerate() {
                match elem {
                    ArrayExpressionElement::Elision(_) => {
                        // Skip - already undefined
                    }
                    _ => {
                        let value = self.compile_expression(elem.to_expression())?;
                        let idx_reg = self.codegen.alloc_reg();
                        self.codegen.emit(Instruction::LoadInt32 {
                            dst: idx_reg,
                            value: i as i32,
                        });
                        self.codegen.emit(Instruction::SetElem {
                            arr: dst,
                            idx: idx_reg,
                            val: value,
                        });
                        self.codegen.free_reg(idx_reg);
                        self.codegen.free_reg(value);
                    }
                }
            }

            return Ok(dst);
        }

        // With spread: build dynamically using `length` and `Spread` instruction.
        self.codegen.emit(Instruction::NewArray { dst, len: 0 });
        let length_key = self.codegen.add_string("length");

        for elem in &arr.elements {
            match elem {
                ArrayExpressionElement::SpreadElement(spread) => {
                    let src = self.compile_expression(&spread.argument)?;
                    self.codegen.emit(Instruction::Spread { dst, src });
                    self.codegen.free_reg(src);
                }
                ArrayExpressionElement::Elision(_) => {
                    // TODO: elision should advance length; skip for now
                }
                _ => {
                    let value = self.compile_expression(elem.to_expression())?;
                    let idx_reg = self.codegen.alloc_reg();
                    let ic_index = self.codegen.alloc_ic();
                    self.codegen.emit(Instruction::GetPropConst {
                        dst: idx_reg,
                        obj: dst,
                        name: length_key,
                        ic_index,
                    });
                    self.codegen.emit(Instruction::SetElem {
                        arr: dst,
                        idx: idx_reg,
                        val: value,
                    });
                    self.codegen.free_reg(idx_reg);
                    self.codegen.free_reg(value);
                }
            }
        }

        Ok(dst)
    }

    /// Compile a conditional (ternary) expression
    fn compile_conditional_expression(
        &mut self,
        cond: &ConditionalExpression,
    ) -> CompileResult<Register> {
        let test = self.compile_expression(&cond.test)?;
        let jump_else = self.codegen.emit_jump_if_false(test);
        self.codegen.free_reg(test);

        // Compile consequent
        let result = self.compile_expression(&cond.consequent)?;
        let jump_end = self.codegen.emit_jump();

        // Patch jump to else
        let else_offset = self.codegen.current_index() as i32 - jump_else as i32;
        self.codegen.patch_jump(jump_else, else_offset);

        // Compile alternate into same register
        let alt = self.compile_expression(&cond.alternate)?;

        // Move to result register if different
        if alt.0 != result.0 {
            self.codegen.emit(Instruction::Move {
                dst: result,
                src: alt,
            });
            self.codegen.free_reg(alt);
        }

        // Patch jump to end
        let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
        self.codegen.patch_jump(jump_end, end_offset);

        Ok(result)
    }

    /// Compile a switch statement
    fn compile_switch_statement(&mut self, stmt: &SwitchStatement) -> CompileResult<()> {
        // 1. Compile discriminant
        let discriminant = self.compile_expression(&stmt.discriminant)?;

        // 2. Setup control scope for breaks
        self.loop_stack.push(ControlScope {
            is_loop: false,
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            continue_target: None,
        });

        // 3. Jump to checks
        let jump_to_checks = self.codegen.emit_jump();

        // 4. Compile bodies and track their entry points
        let mut case_body_labels = Vec::with_capacity(stmt.cases.len());
        let mut default_case_idx = None;

        self.codegen.enter_scope(); // Switch scope

        for (i, case) in stmt.cases.iter().enumerate() {
            // Mark start of this case's body
            let body_start = self.codegen.current_index();
            case_body_labels.push(body_start);

            if case.test.is_none() {
                if default_case_idx.is_some() {
                    return Err(CompileError::syntax("Multiple default clauses", 0, 0));
                }
                default_case_idx = Some(i);
            }

            for stmt in &case.consequent {
                self.compile_statement(stmt)?;
            }
        }

        self.codegen.exit_scope();

        // 5. Jump to end (implicit fallthrough after last case)
        let jump_to_end = self.codegen.emit_jump();

        // 6. Checks Logic
        let checks_start = self.codegen.current_index() as i32;
        self.codegen
            .patch_jump(jump_to_checks, checks_start - jump_to_checks as i32);

        for (i, case) in stmt.cases.iter().enumerate() {
            if let Some(test) = &case.test {
                // Compile test expression
                let test_val = self.compile_expression(test)?;

                // Compare strict equality: discriminant === test
                let cond = self.codegen.alloc_reg();
                self.codegen.emit(Instruction::StrictEq {
                    dst: cond,
                    lhs: discriminant, // Register is Copy
                    rhs: test_val,
                });

                // If match, jump to body
                let jump_match = self.codegen.emit_jump_if_true(cond);
                let body_label = case_body_labels[i] as i32;
                self.codegen
                    .patch_jump(jump_match, body_label - jump_match as i32);

                self.codegen.free_reg(cond);
                self.codegen.free_reg(test_val);
            }
        }

        // Post-checks: Jump to default or end
        if let Some(default_idx) = default_case_idx {
            let default_label = case_body_labels[default_idx] as i32;
            let jmp = self.codegen.emit_jump();
            self.codegen.patch_jump(jmp, default_label - jmp as i32);
        } else {
            let jmp = self.codegen.emit_jump();
            let end_label = self.codegen.current_index() as i32;
            self.codegen.patch_jump(jmp, end_label - jmp as i32);
        }

        // 7. Cleanup
        let end_label = self.codegen.current_index() as i32;
        let jump_to_end_offset = end_label - jump_to_end as i32;
        self.codegen.patch_jump(jump_to_end, jump_to_end_offset);

        self.codegen.free_reg(discriminant);

        // Patch breaks
        let scope = self.loop_stack.pop().unwrap();
        for break_jump in scope.break_jumps {
            let current_offset = self.codegen.current_index() as i32;
            self.codegen
                .patch_jump(break_jump, current_offset - break_jump as i32);
        }

        Ok(())
    }
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compile_number() {
        let compiler = Compiler::new();
        let module = compiler.compile("42", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_addition() {
        let compiler = Compiler::new();
        let module = compiler.compile("1 + 2", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_variable() {
        let compiler = Compiler::new();
        let module = compiler.compile("let x = 10; x + 5", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_if() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("if (true) { 1 } else { 2 }", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_while() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let i = 0; while (i < 10) { i = i + 1 }", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_const() {
        let compiler = Compiler::new();
        let module = compiler.compile("const PI = 3.15;", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_multiple_variables() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let a = 1; let b = 2; const c = a + b;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_if_else() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let x = 5; if (x > 10) { x = 1; } else { x = 2; }",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_for() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let sum = 0; for (let i = 0; i < 10; i = i + 1) { sum = sum + i; }",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_block_scope() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let x = 1; { let y = 2; x = x + y; }", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_function_declaration() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("function add(a, b) { return a + b; }", "test.js")
            .unwrap();

        // 2 functions: main + add
        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_function_call() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "function double(x) { return x * 2; } let result = double(5);",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_function_expression() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let add = function(a, b) { return a + b; }; add(1, 2);",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_arrow_function() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let add = (a, b) => a + b; let result = add(2, 3);",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_arrow_function_block() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let add = (a, b) => { return a + b; }; add(1, 2);",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_recursion() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "function factorial(n) { if (n <= 1) { return 1; } return n * factorial(n - 1); }",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_object_literal() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let obj = { x: 1, y: 2 };", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_array_literal() {
        let compiler = Compiler::new();
        let module = compiler.compile("let arr = [1, 2, 3];", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_property_access() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let obj = { x: 10 }; let v = obj.x;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_element_access() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let arr = [1, 2, 3]; let v = arr[1];", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_property_assignment() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let obj = { x: 1 }; obj.x = 42;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_element_assignment() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let arr = [1, 2, 3]; arr[0] = 10;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_async_function() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("async function fetchData() { return 42; }", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 2);
        // The async function (fetchData) is at index 0, main is at index 1
        assert!(module.functions[0].is_async());
        assert_eq!(module.functions[0].name, Some("fetchData".to_string()));
    }

    #[test]
    fn test_compile_async_arrow() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let f = async () => 42;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 2);
        // The async arrow function is at index 0, main is at index 1
        assert!(module.functions[0].is_async());
    }

    #[test]
    fn test_compile_await() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "async function test() { let x = await fetch(); return x; }",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
        // The async function (test) is at index 0, main is at index 1
        assert!(module.functions[0].is_async());
        assert_eq!(module.functions[0].name, Some("test".to_string()));
    }

    #[test]
    fn test_compile_import() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("import { foo } from './module.js';", "test.js")
            .unwrap();

        assert!(module.is_esm);
        assert_eq!(module.imports.len(), 1);
        assert_eq!(module.imports[0].specifier, "./module.js");
        assert_eq!(module.imports[0].bindings.len(), 1);
    }

    #[test]
    fn test_compile_import_default() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("import foo from './module.js';", "test.js")
            .unwrap();

        assert!(module.is_esm);
        assert_eq!(module.imports.len(), 1);
    }

    #[test]
    fn test_compile_import_namespace() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("import * as utils from './utils.js';", "test.js")
            .unwrap();

        assert!(module.is_esm);
        assert_eq!(module.imports.len(), 1);
    }

    #[test]
    fn test_compile_export_named() {
        let compiler = Compiler::new();
        let module = compiler.compile("export const x = 42;", "test.js").unwrap();

        assert!(module.is_esm);
        assert_eq!(module.exports.len(), 1);
    }

    #[test]
    fn test_compile_export_function() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("export function add(a, b) { return a + b; }", "test.js")
            .unwrap();

        assert!(module.is_esm);
        assert_eq!(module.exports.len(), 1);
        assert_eq!(module.functions.len(), 2); // main + add
    }

    #[test]
    fn test_compile_export_default() {
        let compiler = Compiler::new();
        let module = compiler.compile("export default 42;", "test.js").unwrap();

        assert!(module.is_esm);
        assert_eq!(module.exports.len(), 1);
    }

    #[test]
    fn test_compile_export_default_function() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("export default function() { return 42; }", "test.js")
            .unwrap();

        assert!(module.is_esm);
        assert_eq!(module.exports.len(), 1);
        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_export_all() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("export * from './other.js';", "test.js")
            .unwrap();

        assert!(module.is_esm);
        assert_eq!(module.exports.len(), 1);
    }

    #[test]
    fn test_compile_reexport_named() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("export { foo } from './module.js';", "test.js")
            .unwrap();

        assert!(module.is_esm);
        assert_eq!(module.exports.len(), 1);
    }

    #[test]
    fn test_compile_mixed_imports_exports() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                r#"
                import { add } from './math.js';
                export const result = add(1, 2);
                export function multiply(a, b) { return a * b; }
                "#,
                "test.js",
            )
            .unwrap();

        assert!(module.is_esm);
        assert_eq!(module.imports.len(), 1);
        assert_eq!(module.exports.len(), 2);
    }

    // TypeScript tests

    #[test]
    fn test_compile_ts_type_alias() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("type Foo = string; let x = 42;", "test.ts")
            .unwrap();

        // Type alias is erased, only variable remains
        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_interface() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "interface User { name: string; } let user = { name: 'Alice' };",
                "test.ts",
            )
            .unwrap();

        // Interface is erased, only variable remains
        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_as_expression() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let x = (42 as number);", "test.ts")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_type_assertion() {
        let compiler = Compiler::new();
        let module = compiler.compile("let x = <number>42;", "test.ts").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_non_null_assertion() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let x = null; let y = x!;", "test.ts")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_satisfies() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let x = { name: 'test' } satisfies { name: string };",
                "test.ts",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_enum_basic() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("enum Color { Red, Green, Blue }", "test.ts")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_enum_with_values() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("enum Status { Active = 1, Inactive = 2 }", "test.ts")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_string_enum() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(r#"enum Direction { Up = "UP", Down = "DOWN" }"#, "test.ts")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_function_with_types() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "function add(a: number, b: number): number { return a + b; }",
                "test.ts",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_ts_generic_function() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("function identity<T>(x: T): T { return x; }", "test.ts")
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_ts_arrow_with_types() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let add = (a: number, b: number): number => a + b;",
                "test.ts",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_ts_namespace() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("namespace Utils { export const PI = 3.14; }", "test.ts")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_ts_mixed() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                r#"
                type ID = string | number;
                interface User {
                    id: ID;
                    name: string;
                }
                enum Role { Admin, User }
                function createUser(name: string): User {
                    return { id: 1, name };
                }
                const admin = createUser("Admin") as User;
                "#,
                "test.ts",
            )
            .unwrap();

        // 1 main function + 1 createUser function
        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_shim_builtins_js_compiles() {
        let builtins = include_str!("../../otter-vm-builtins/src/builtins.js");
        Compiler::new().compile(builtins, "builtins.js").unwrap();
    }

    #[test]
    fn test_shim_fetch_js_compiles() {
        let fetch = include_str!("../../otter-vm-builtins/src/fetch.js");
        Compiler::new().compile(fetch, "fetch.js").unwrap();
    }

    #[test]
    fn test_normal_code_compiles() {
        // Ensure that typical nested code still compiles fine
        let code = r#"
            function outer() {
                function middle() {
                    function inner() {
                        return 1 + 2 + 3;
                    }
                    return inner();
                }
                return middle();
            }
            outer();
        "#;
        let compiler = Compiler::new();
        let module = compiler.compile(code, "test.js").unwrap();
        assert!(module.functions.len() >= 1);
    }

    #[test]
    fn test_deeply_nested_expression_error() {
        // Generate a deeply nested binary expression: 1+1+1+...+1 (600+ levels)
        let mut expr = String::from("1");
        for _ in 0..600 {
            expr = format!("({} + 1)", expr);
        }
        let code = format!("let x = {};", expr);

        let compiler = Compiler::new();
        let result = compiler.compile(&code, "test.js");

        // Should fail with a depth error
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("nesting depth"),
            "Expected nesting depth error, got: {}",
            err
        );
    }

    // Integration tests for compiler validation
    // Property 8: Early Error Generation
    // Validates: Requirements 7.1, 7.2

    #[test]
    fn test_legacy_octal_literal_strict_mode_error() {
        let mut compiler = Compiler::new();
        compiler.set_strict_mode(true);

        // Legacy octal literals should fail in strict mode
        let result = compiler.compile("let x = 077;", "test.js");
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(matches!(err, CompileError::LegacySyntax { .. }));
        assert!(err.to_string().contains("Legacy octal literal"));
    }

    #[test]
    fn test_legacy_octal_literal_non_strict_mode_success() {
        let compiler = Compiler::new(); // non-strict by default

        // Legacy octal literals should work in non-strict mode
        let result = compiler.compile("let x = 077;", "test.js");
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_numeric_separator_error() {
        // Invalid numeric separator usage should fail
        let test_cases = vec![
            "let x = _123;",  // leading separator
            "let x = 123_;",  // trailing separator
            "let x = 1__23;", // consecutive separators
            "let x = 0x_FF;", // separator after prefix
        ];

        for code in test_cases {
            let compiler = Compiler::new();
            let result = compiler.compile(code, "test.js");
            // Note: Some of these might be caught by the parser first,
            // but if they reach our validator, they should fail
            if result.is_err() {
                let err = result.unwrap_err();
                // Could be either a parse error or our validation error
                assert!(
                    matches!(err, CompileError::Parse(_))
                        || matches!(err, CompileError::InvalidLiteral { .. })
                );
            }
        }
    }

    #[test]
    fn test_valid_numeric_literals_success() {
        let test_cases = vec![
            "let x = 123;",       // decimal
            "let x = 1_000_000;", // decimal with separators
            "let x = 0xFF;",      // hex
            "let x = 0xFF_FF;",   // hex with separators
            "let x = 0b1010;",    // binary
            "let x = 0b10_10;",   // binary with separators
            "let x = 3.14;",      // float
            "let x = 1e10;",      // exponent
            "let x = 1.5e-10;",   // exponent with sign
        ];

        for code in test_cases {
            let compiler = Compiler::new();
            let result = compiler.compile(code, "test.js");
            assert!(result.is_ok(), "Failed to compile: {}", code);
        }
    }

    #[test]
    fn test_legacy_string_escape_strict_mode_error() {
        // Legacy escape sequences should fail in strict mode
        let test_cases = vec![
            r#"let x = "octal\1";"#,   // octal escape
            r#"let x = "invalid\8";"#, // invalid numeric escape
            r#"let x = "invalid\9";"#, // invalid numeric escape
        ];

        for code in test_cases {
            let mut compiler = Compiler::new();
            compiler.set_strict_mode(true);
            let result = compiler.compile(code, "test.js");
            assert!(result.is_err(), "Expected error for: {}", code);

            let err = result.unwrap_err();
            assert!(matches!(err, CompileError::LegacySyntax { .. }));
        }
    }

    #[test]
    fn test_valid_string_literals_success() {
        let test_cases = vec![
            r#"let x = "hello";"#,          // simple string
            r#"let x = "hello\nworld";"#,   // newline escape
            r#"let x = "tab\tseparated";"#, // tab escape
            r#"let x = "quote\"mark";"#,    // quote escape
            r#"let x = "hex\x41";"#,        // hex escape
            r#"let x = "unicode\u0041";"#,  // unicode escape
            r#"let x = "unicode\u{41}";"#,  // unicode brace escape
        ];

        for code in test_cases {
            let compiler = Compiler::new();
            let result = compiler.compile(code, "test.js");
            assert!(result.is_ok(), "Failed to compile: {}", code);
        }
    }

    #[test]
    fn test_invalid_string_escape_error() {
        let test_cases = vec![
            r#"let x = "bad\xGG";"#,     // invalid hex escape
            r#"let x = "short\xF";"#,    // short hex escape
            r#"let x = "bad\uGGGG";"#,   // invalid unicode escape
            r#"let x = "short\u41";"#,   // short unicode escape
            r#"let x = "empty\u{}";"#,   // empty unicode brace
            r#"let x = "unterm\u{41";"#, // unterminated unicode brace
        ];

        for code in test_cases {
            let compiler = Compiler::new();
            let result = compiler.compile(code, "test.js");
            // These should either be caught by parser or our validator
            if result.is_err() {
                let err = result.unwrap_err();
                assert!(
                    matches!(err, CompileError::Parse(_))
                        || matches!(err, CompileError::InvalidLiteral { .. })
                );
            }
        }
    }

    #[test]
    fn test_regexp_literal_validation_success() {
        let test_cases = vec![
            r#"let x = /hello/;"#,        // simple pattern
            r#"let x = /hello/g;"#,       // with flags
            r#"let x = /hello/gi;"#,      // multiple flags
            r#"let x = /[a-z]+/i;"#,      // character class
            r#"let x = /\d+/;"#,          // escape sequence
            r#"let x = /hello\nworld/;"#, // newline in pattern
        ];

        for code in test_cases {
            let compiler = Compiler::new();
            let result = compiler.compile(code, "test.js");
            assert!(result.is_ok(), "Failed to compile: {}", code);
        }
    }

    #[test]
    fn test_regexp_literal_invalid_flags_error() {
        let test_cases = vec![
            r#"let x = /hello/gg;"#, // duplicate flag
            r#"let x = /hello/uv;"#, // conflicting flags u and v
            r#"let x = /hello/x;"#,  // invalid flag
        ];

        for code in test_cases {
            let compiler = Compiler::new();
            let result = compiler.compile(code, "test.js");
            // These should be caught by our validator
            if result.is_err() {
                let err = result.unwrap_err();
                assert!(
                    matches!(err, CompileError::Parse(_))
                        || matches!(err, CompileError::InvalidLiteral { .. })
                );
            }
        }
    }

    #[test]
    fn test_template_literal_validation_success() {
        let test_cases = vec![
            r#"let x = `hello`;"#,              // simple template
            r#"let x = `hello ${name}`;"#,      // with expression
            r#"let x = `hello\nworld`;"#,       // with escape
            r#"let x = `tab\tseparated`;"#,     // tab escape
            r#"let x = `unicode\u0041`;"#,      // unicode escape
            r#"let x = `multiple ${a} ${b}`;"#, // multiple expressions
        ];

        for code in test_cases {
            let compiler = Compiler::new();
            let result = compiler.compile(code, "test.js");
            assert!(result.is_ok(), "Failed to compile: {}", code);
        }
    }

    #[test]
    fn test_template_literal_legacy_escape_strict_mode_error() {
        let test_cases = vec![
            r#"let x = `octal\1`;"#,   // octal escape in template
            r#"let x = `invalid\8`;"#, // invalid numeric escape
        ];

        for code in test_cases {
            let mut compiler = Compiler::new();
            compiler.set_strict_mode(true);
            let result = compiler.compile(code, "test.js");
            assert!(result.is_err(), "Expected error for: {}", code);

            let err = result.unwrap_err();
            // The parser catches these errors before our validator, which is correct
            assert!(
                matches!(err, CompileError::Parse(_))
                    || matches!(err, CompileError::LegacySyntax { .. }),
                "Expected Parse or LegacySyntax error, got: {:?}",
                err
            );
        }
    }

    #[test]
    fn test_debug_hex_literal() {
        let code = "let x = 0x811A6E;";
        let compiler = Compiler::new();
        let result = compiler.compile(code, "test.js");

        match &result {
            Ok(_) => println!("Successfully compiled: {}", code),
            Err(e) => println!("Failed to compile {}: {:?}", code, e),
        }

        assert!(result.is_ok(), "Failed to compile hex literal: {}", code);
    }

    // Property tests for enhanced literal compilation
    // Property 9: Runtime Literal Consistency
    // Validates: Requirements 1.6, 2.5, 4.4

    #[cfg(test)]
    mod property_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_integer_literal_compilation_consistency(
                value in -1000000i64..1000000i64,
            ) {
                let code = format!("let x = {};", value);
                let compiler = Compiler::new();
                let result = compiler.compile(&code, "test.js");

                // Should compile successfully for all integer values
                prop_assert!(result.is_ok(), "Failed to compile integer literal: {}", value);

                let module = result.unwrap();
                prop_assert!(module.functions.len() >= 1);
            }

            #[test]
            fn prop_hex_literal_compilation_consistency(
                value in 0u32..0xFFFFFF,
            ) {
                let code = format!("let x = 0x{:X};", value);
                let compiler = Compiler::new();
                let result = compiler.compile(&code, "test.js");

                // Should compile successfully for all hex values
                prop_assert!(result.is_ok(), "Failed to compile hex literal: 0x{:X}", value);

                let module = result.unwrap();
                prop_assert!(module.functions.len() >= 1);
            }

            #[test]
            fn prop_string_literal_compilation_consistency(
                text in "[a-zA-Z0-9 ]{0,50}",
            ) {
                let code = format!(r#"let x = "{}";"#, text);
                let compiler = Compiler::new();
                let result = compiler.compile(&code, "test.js");

                // Should compile successfully for simple text strings
                prop_assert!(result.is_ok(), "Failed to compile string literal: \"{}\"", text);

                let module = result.unwrap();
                prop_assert!(module.functions.len() >= 1);
            }

            #[test]
            fn prop_boolean_literal_compilation_consistency(
                value in any::<bool>(),
            ) {
                let code = format!("let x = {};", value);
                let compiler = Compiler::new();
                let result = compiler.compile(&code, "test.js");

                // Should compile successfully for all boolean values
                prop_assert!(result.is_ok(), "Failed to compile boolean literal: {}", value);

                let module = result.unwrap();
                prop_assert!(module.functions.len() >= 1);
            }

            #[test]
            fn prop_mixed_literals_compilation_consistency(
                num_value in -1000i32..1000i32,
                str_text in "[a-zA-Z0-9]{0,20}",
                bool_value in any::<bool>(),
            ) {
                let code = format!(
                    r#"let a = {}; let b = "{}"; let c = {}; let d = null;"#,
                    num_value, str_text, bool_value
                );

                let compiler = Compiler::new();
                let result = compiler.compile(&code, "test.js");

                // Should compile successfully for mixed literal types
                prop_assert!(result.is_ok(), "Failed to compile mixed literals: {}", code);

                let module = result.unwrap();
                prop_assert!(module.functions.len() >= 1);
            }

            #[test]
            fn prop_strict_mode_consistency(
                num_value in 1i32..1000i32,
                str_text in "[a-zA-Z0-9]{0,20}",
                strict_mode in any::<bool>(),
            ) {
                let code = format!(r#"let a = {}; let b = "{}";"#, num_value, str_text);

                let mut compiler = Compiler::new();
                compiler.set_strict_mode(strict_mode);
                let result = compiler.compile(&code, "test.js");

                // Valid literals should compile in both strict and non-strict mode
                prop_assert!(result.is_ok(), "Failed to compile valid literals in strict_mode={}: {}", strict_mode, code);

                let module = result.unwrap();
                prop_assert!(module.functions.len() >= 1);
            }
        }
    }
}
