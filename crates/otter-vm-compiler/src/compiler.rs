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
use crate::scope::ResolvedBinding;

/// The compiler
pub struct Compiler {
    /// Code generator
    codegen: CodeGen,
}

impl Compiler {
    /// Create a new compiler
    pub fn new() -> Self {
        Self {
            codegen: CodeGen::new(),
        }
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
        for stmt in &program.body {
            self.compile_statement(stmt)?;
        }
        Ok(())
    }

    /// Compile a statement
    fn compile_statement(&mut self, stmt: &Statement) -> CompileResult<()> {
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

            Statement::FunctionDeclaration(func) => self.compile_function_declaration(func),

            Statement::EmptyStatement(_) => Ok(()),

            Statement::DebuggerStatement(_) => {
                self.codegen.emit(Instruction::Debugger);
                Ok(())
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

            _ => Err(CompileError::unsupported("Unknown statement type")),
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
                    // TODO: Add class compilation when we have class support
                    if let Some(id) = &class.id {
                        let name = id.name.to_string();
                        self.codegen.add_export(ExportRecord::Named {
                            local: name.clone(),
                            exported: name,
                        });
                    }
                    return Err(CompileError::unsupported("Class declarations"));
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
                for param in &func.params.items {
                    if let BindingPattern::BindingIdentifier(ident) = &param.pattern {
                        self.codegen.declare_variable(&ident.name, false)?;
                        self.codegen.current.param_count += 1;
                    }
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

            ExportDefaultDeclarationKind::ClassDeclaration(_) => {
                return Err(CompileError::unsupported("Class declarations"));
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
                self.codegen.emit(Instruction::SetPropConst {
                    obj: enum_obj,
                    name: name_idx,
                    val: val_reg,
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
                    self.codegen.emit(Instruction::SetProp {
                        obj: enum_obj,
                        key: val_reg,
                        val: str_val,
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
                self.codegen.emit(Instruction::SetPropConst {
                    obj: enum_obj,
                    name: name_idx,
                    val: val_reg,
                });

                // Set reverse mapping: Color[0] = "Red"
                let str_val = self.codegen.alloc_reg();
                let str_idx = self.codegen.add_string(&member_name);
                self.codegen.emit(Instruction::LoadConst {
                    dst: str_val,
                    idx: str_idx,
                });
                self.codegen.emit(Instruction::SetProp {
                    obj: enum_obj,
                    key: val_reg,
                    val: str_val,
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
                                                    self.codegen.emit(Instruction::SetPropConst {
                                                        obj: ns_obj,
                                                        name: name_idx,
                                                        val,
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
                                                self.codegen.emit(Instruction::SetPropConst {
                                                    obj: ns_obj,
                                                    name: name_idx,
                                                    val,
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
        let is_const = decl.kind == VariableDeclarationKind::Const;

        for declarator in &decl.declarations {
            match &declarator.id {
                BindingPattern::BindingIdentifier(ident) => {
                    let local_idx = self.codegen.declare_variable(&ident.name, is_const)?;

                    if let Some(init) = &declarator.init {
                        let reg = self.compile_expression(init)?;
                        self.codegen.emit(Instruction::SetLocal {
                            idx: LocalIndex(local_idx),
                            src: reg,
                        });
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
        if let Some(update) = &for_stmt.update {
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

        self.codegen.exit_scope();
        Ok(())
    }

    /// Compile a function declaration
    fn compile_function_declaration(&mut self, func: &oxc_ast::ast::Function) -> CompileResult<()> {
        let name = func.id.as_ref().map(|id| id.name.to_string());
        let is_async = func.r#async;

        // Declare function in current scope
        if let Some(ref n) = name {
            self.codegen.declare_variable(n, false)?;
        }

        // Enter function context
        self.codegen.enter_function(name.clone());
        self.codegen.current.flags.is_async = is_async;

        // Declare parameters
        for param in &func.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
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

        // Compile function body
        if let Some(body) = &func.body {
            for stmt in &body.statements {
                self.compile_statement(stmt)?;
            }
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
            self.codegen.emit(Instruction::SetLocal {
                idx: LocalIndex(idx),
                src: dst,
            });
            self.codegen.free_reg(dst);
        }

        Ok(())
    }

    /// Compile a function expression
    fn compile_function_expression(
        &mut self,
        func: &oxc_ast::ast::Function,
    ) -> CompileResult<Register> {
        let name = func.id.as_ref().map(|id| id.name.to_string());
        let is_async = func.r#async;

        // Enter function context
        self.codegen.enter_function(name);
        self.codegen.current.flags.is_async = is_async;

        // Declare parameters
        for param in &func.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
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

        // Compile function body
        if let Some(body) = &func.body {
            for stmt in &body.statements {
                self.compile_statement(stmt)?;
            }
        }

        // Ensure return
        self.codegen.emit(Instruction::ReturnUndefined);

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

        Ok(dst)
    }

    /// Compile an arrow function expression
    fn compile_arrow_function(
        &mut self,
        arrow: &ArrowFunctionExpression,
    ) -> CompileResult<Register> {
        let is_async = arrow.r#async;

        // Enter function context
        self.codegen.enter_function(None);
        self.codegen.current.flags.is_arrow = true;
        self.codegen.current.flags.is_async = is_async;

        // Declare parameters
        for param in &arrow.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
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

        // Compile body
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

        Ok(dst)
    }

    /// Compile an expression
    fn compile_expression(&mut self, expr: &Expression) -> CompileResult<Register> {
        match expr {
            Expression::NumericLiteral(lit) => {
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
                let dst = self.codegen.alloc_reg();
                let idx = self.codegen.add_string(&lit.value);
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

            // TypeScript expressions - type erasure (compile inner expression, ignore type)
            Expression::TSAsExpression(expr) => self.compile_expression(&expr.expression),

            Expression::TSSatisfiesExpression(expr) => self.compile_expression(&expr.expression),

            Expression::TSTypeAssertion(expr) => self.compile_expression(&expr.expression),

            Expression::TSNonNullExpression(expr) => self.compile_expression(&expr.expression),

            Expression::TSInstantiationExpression(expr) => {
                self.compile_expression(&expr.expression)
            }

            _ => Err(CompileError::unsupported("Unknown expression type")),
        }
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
                self.codegen.emit(Instruction::GetGlobal {
                    dst,
                    name: name_idx,
                });
            }
            Some(ResolvedBinding::Upvalue { .. }) => {
                return Err(CompileError::unsupported("Upvalues"));
            }
            None => {
                let name_idx = self.codegen.add_string(name);
                self.codegen.emit(Instruction::GetGlobal {
                    dst,
                    name: name_idx,
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

    /// Compile a unary expression
    fn compile_unary_expression(&mut self, unary: &UnaryExpression) -> CompileResult<Register> {
        let src = self.compile_expression(&unary.argument)?;
        let dst = self.codegen.alloc_reg();

        let instruction = match unary.operator {
            UnaryOperator::UnaryNegation => Instruction::Neg { dst, src },
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
                        self.codegen.emit(Instruction::SetGlobal {
                            name: name_idx,
                            src: value,
                        });
                    }
                    Some(ResolvedBinding::Upvalue { .. }) => {
                        return Err(CompileError::unsupported("Upvalue assignment"));
                    }
                }
            }
            AssignmentTarget::StaticMemberExpression(member) => {
                let obj = self.compile_expression(&member.object)?;
                let name_idx = self.codegen.add_string(&member.property.name);
                self.codegen.emit(Instruction::SetPropConst {
                    obj,
                    name: name_idx,
                    val: value,
                });
                self.codegen.free_reg(obj);
            }
            AssignmentTarget::ComputedMemberExpression(member) => {
                let obj = self.compile_expression(&member.object)?;
                let key = self.compile_expression(&member.expression)?;
                self.codegen.emit(Instruction::SetProp {
                    obj,
                    key,
                    val: value,
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
            // Arguments MUST be at func+1, func+2, ... for the Call instruction
            let argc = call.arguments.len() as u8;

            // Reserve registers for arguments right after func
            let mut reserved = Vec::with_capacity(call.arguments.len());
            for _ in 0..call.arguments.len() {
                reserved.push(self.codegen.alloc_reg());
            }

            // Compile each argument and move to its designated register
            for (i, arg) in call.arguments.iter().enumerate() {
                let temp = self.compile_expression(arg.to_expression())?;
                let target = Register(func.0 + 1 + i as u8);
                if temp.0 != target.0 {
                    self.codegen.emit(Instruction::Move { dst: target, src: temp });
                    self.codegen.free_reg(temp);
                }
            }

            // Allocate dst register (will be after all args)
            let dst = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::Call { dst, func, argc });

            // Free func and reserved arg registers
            self.codegen.free_reg(func);
            for reg in reserved {
                self.codegen.free_reg(reg);
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
        self.codegen
            .emit(Instruction::NewArray { dst: args_arr, len: 0 });

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
                    self.codegen.emit(Instruction::GetPropConst {
                        dst: len_reg,
                        obj: args_arr,
                        name: len_name,
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

        // Check if we have any spread arguments
        let has_spread = new_expr
            .arguments
            .iter()
            .any(|arg| matches!(arg, Argument::SpreadElement(_)));

        if has_spread {
            // For now, spread in new expressions is not supported
            // This would require a ConstructSpread instruction
            return Err(CompileError::unsupported("Spread in new expressions"));
        }

        // Compile arguments
        let argc = new_expr.arguments.len() as u8;
        let mut arg_regs = Vec::with_capacity(new_expr.arguments.len());

        for arg in &new_expr.arguments {
            let reg = self.compile_expression(arg.to_expression())?;
            arg_regs.push(reg);
        }

        // Free argument registers
        for reg in arg_regs.iter().rev() {
            self.codegen.free_reg(*reg);
        }

        let dst = self.codegen.alloc_reg();
        self.codegen
            .emit(Instruction::Construct { dst, func, argc });
        self.codegen.free_reg(func);

        Ok(dst)
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
                self.codegen.emit(Instruction::SetGlobal {
                    name: name_idx,
                    src,
                });
            }
            Some(ResolvedBinding::Upvalue { .. }) => {
                return Err(CompileError::unsupported("Upvalues"));
            }
            None => {
                // Undeclared variable - treat as global
                let name_idx = self.codegen.add_string(name);
                self.codegen.emit(Instruction::SetGlobal {
                    name: name_idx,
                    src,
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
        self.codegen.emit(Instruction::GetPropConst {
            dst,
            obj,
            name: name_idx,
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
        self.codegen.emit(Instruction::GetProp { dst, obj, key });
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
                    let key = match &prop.key {
                        PropertyKey::StaticIdentifier(ident) => {
                            self.codegen.add_string(&ident.name)
                        }
                        PropertyKey::StringLiteral(lit) => self.codegen.add_string(&lit.value),
                        _ => return Err(CompileError::unsupported("Computed property keys")),
                    };

                    let value = self.compile_expression(&prop.value)?;
                    self.codegen.emit(Instruction::SetPropConst {
                        obj: dst,
                        name: key,
                        val: value,
                    });
                    self.codegen.free_reg(value);
                }
                ObjectPropertyKind::SpreadProperty(_) => {
                    return Err(CompileError::unsupported("Object spread"));
                }
            }
        }

        Ok(dst)
    }

    /// Compile an array expression
    fn compile_array_expression(&mut self, arr: &ArrayExpression) -> CompileResult<Register> {
        let len = arr.elements.len() as u16;
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::NewArray { dst, len });

        for (i, elem) in arr.elements.iter().enumerate() {
            match elem {
                ArrayExpressionElement::SpreadElement(_) => {
                    return Err(CompileError::unsupported("Array spread"));
                }
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
        self.codegen.free_reg(result);
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
        // The async function should have is_async flag
        assert!(module.functions[1].is_async());
    }

    #[test]
    fn test_compile_async_arrow() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let f = async () => 42;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 2);
        assert!(module.functions[1].is_async());
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
        assert!(module.functions[1].is_async());
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
}
