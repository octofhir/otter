//! §16.2 — Import and Export declaration compilation.
//!
//! Handles `import`, `export`, `export default`, and `export *` declarations,
//! producing `ImportRecord` / `ExportRecord` metadata on the compiled module
//! and declaring local bindings for imported names.
//!
//! Spec: <https://tc39.es/ecma262/#sec-modules>

use oxc_ast::ast::{
    BindingPattern, ExportAllDeclaration, ExportDefaultDeclaration, ExportDefaultDeclarationKind,
    ExportNamedDeclaration, ImportDeclaration, ImportDeclarationSpecifier,
};

use crate::bytecode::Instruction;
use crate::module::{ExportRecord, ImportBinding, ImportRecord};
use crate::source::SourceLoweringError;

use super::module_compiler::ModuleCompiler;
use super::shared::FunctionCompiler;

impl<'a> FunctionCompiler<'a> {
    /// §16.2.2 — Compile an `import` declaration.
    ///
    /// ```js
    /// import { foo, bar as baz } from "./module.js";
    /// import defaultExport from "./module.js";
    /// import * as ns from "./module.js";
    /// import "./side-effect.js";  // side-effect only
    /// ```
    ///
    /// Each import binding is declared as a local variable. The host runtime
    /// populates these locals before module evaluation begins.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-imports>
    pub(super) fn compile_import_declaration(
        &mut self,
        import: &ImportDeclaration<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        // Skip type-only imports (TypeScript).
        if import.import_kind.is_type() {
            return Ok(());
        }

        let specifier: Box<str> = import.source.value.as_str().into();
        let mut bindings = Vec::new();

        if let Some(specifiers) = &import.specifiers {
            for spec in specifiers {
                match spec {
                    // import { imported as local } from "..."
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        if s.import_kind.is_type() {
                            continue;
                        }
                        let imported_name = s.imported.name().as_str();
                        let local_name = s.local.name.as_str();

                        // In module mode, import bindings are pre-populated as
                        // globals by the host before evaluation. The compiler
                        // does NOT declare a local — the name resolves via
                        // GetGlobal at the use site.
                        // In non-module mode (e.g. bundler output), declare a local.
                        if module.mode() != crate::source::LoweringMode::Module {
                            self.declare_import_binding(local_name)?;
                        }

                        bindings.push(ImportBinding::Named {
                            imported: imported_name.into(),
                            local: local_name.into(),
                        });
                    }
                    // import local from "..."
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                        let local_name = s.local.name.as_str();
                        if module.mode() != crate::source::LoweringMode::Module {
                            self.declare_import_binding(local_name)?;
                        }

                        bindings.push(ImportBinding::Default {
                            local: local_name.into(),
                        });
                    }
                    // import * as local from "..."
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        let local_name = s.local.name.as_str();
                        if module.mode() != crate::source::LoweringMode::Module {
                            self.declare_import_binding(local_name)?;
                        }

                        bindings.push(ImportBinding::Namespace {
                            local: local_name.into(),
                        });
                    }
                }
            }
        }

        // Record the import for module linking even if there are no bindings
        // (side-effect-only imports like `import "./init.js"`).
        module.add_import(ImportRecord {
            specifier,
            bindings,
        });

        Ok(())
    }

    /// §16.2.3 — Compile an `export` named declaration.
    ///
    /// Three patterns:
    /// 1. Re-export: `export { foo } from "./module.js"`
    /// 2. Local export: `export { foo, bar as baz }`
    /// 3. Declaration: `export const x = 1` / `export function f() {}`
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-exports>
    pub(super) fn compile_export_named_declaration(
        &mut self,
        export: &ExportNamedDeclaration<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        // Skip type-only exports (TypeScript).
        if export.export_kind.is_type() {
            return Ok(());
        }

        // Pattern 1: Re-export — `export { foo } from "./module.js"`
        if let Some(source) = &export.source {
            let specifier: Box<str> = source.value.as_str().into();
            for spec in &export.specifiers {
                let imported = spec.local.name().as_str().into();
                let exported = spec.exported.name().as_str().into();
                module.add_export(ExportRecord::ReExportNamed {
                    specifier: specifier.clone(),
                    imported,
                    exported,
                });
            }
            return Ok(());
        }

        // Pattern 2: Local export specifiers — `export { foo, bar as baz }`
        if !export.specifiers.is_empty() {
            for spec in &export.specifiers {
                if spec.export_kind.is_type() {
                    continue;
                }
                let local: Box<str> = spec.local.name().as_str().into();
                let exported: Box<str> = spec.exported.name().as_str().into();
                module.add_export(ExportRecord::Named { local, exported });
            }
            return Ok(());
        }

        // Pattern 3: Declaration — `export const x = 1` / `export function f() {}`
        if let Some(declaration) = &export.declaration {
            match declaration {
                oxc_ast::ast::Declaration::VariableDeclaration(var_decl) => {
                    // Compile the variable declaration normally.
                    self.compile_variable_declaration(var_decl, module)?;
                    // Record each declarator name as a named export.
                    for declarator in &var_decl.declarations {
                        self.collect_binding_export_names(&declarator.id, module);
                    }
                }
                oxc_ast::ast::Declaration::FunctionDeclaration(func) => {
                    // Function declarations are hoisted — already compiled.
                    // Just record the export.
                    if let Some(id) = func.id.as_ref() {
                        let name: Box<str> = id.name.as_str().into();
                        module.add_export(ExportRecord::Named {
                            local: name.clone(),
                            exported: name,
                        });
                    }
                }
                oxc_ast::ast::Declaration::ClassDeclaration(class) => {
                    self.compile_class_declaration(class, module)?;
                    if let Some(id) = class.id.as_ref() {
                        let name: Box<str> = id.name.as_str().into();
                        module.add_export(ExportRecord::Named {
                            local: name.clone(),
                            exported: name,
                        });
                    }
                }
                _ => {
                    return Err(SourceLoweringError::Unsupported(
                        "unsupported export declaration kind".into(),
                    ));
                }
            }
        }

        Ok(())
    }

    /// §16.2.3 — Compile an `export default` declaration.
    ///
    /// ```js
    /// export default function() { ... }
    /// export default class { ... }
    /// export default expression;
    /// ```
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-exports>
    pub(super) fn compile_export_default_declaration(
        &mut self,
        export: &ExportDefaultDeclaration<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                // Named function: `export default function foo() {}`
                // Anonymous function: `export default function() {}`
                let local_name = func
                    .id
                    .as_ref()
                    .map(|id| id.name.as_str())
                    .unwrap_or("*default*");

                // If named, it's already hoisted. If anonymous, we need to
                // declare the binding and compile it.
                if func.id.is_none() {
                    let binding_reg = self.declare_import_binding("*default*")?;
                    let value = self.compile_function_expression(func, Some("default"), module)?;
                    self.instructions.push(crate::bytecode::Instruction::move_(
                        binding_reg,
                        value.register,
                    ));
                    self.release(value);
                }

                module.add_export(ExportRecord::Default {
                    local: local_name.into(),
                });
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                let local_name = class
                    .id
                    .as_ref()
                    .map(|id| id.name.as_str())
                    .unwrap_or("*default*");

                if class.id.is_some() {
                    // Named class — compile it as a declaration.
                    self.compile_class_declaration(class, module)?;
                } else {
                    // Anonymous class — compile as expression and store in binding.
                    let binding_reg = self.declare_import_binding("*default*")?;
                    let value = self.compile_class_expression(class, module)?;
                    self.instructions.push(crate::bytecode::Instruction::move_(
                        binding_reg,
                        value.register,
                    ));
                    self.release(value);
                }

                module.add_export(ExportRecord::Default {
                    local: local_name.into(),
                });
            }
            _ => {
                // Expression: `export default expr`
                if let Some(expr) = export.declaration.as_expression() {
                    let binding_reg = self.declare_import_binding("*default*")?;
                    let value = self.compile_expression(expr, module)?;
                    self.instructions.push(crate::bytecode::Instruction::move_(
                        binding_reg,
                        value.register,
                    ));
                    self.release(value);

                    module.add_export(ExportRecord::Default {
                        local: "*default*".into(),
                    });
                } else {
                    return Err(SourceLoweringError::Unsupported(
                        "unsupported export default declaration kind".into(),
                    ));
                }
            }
        }

        Ok(())
    }

    /// §16.2.3 — Compile an `export *` declaration.
    ///
    /// ```js
    /// export * from "./module.js";
    /// export * as ns from "./module.js";
    /// ```
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-exports>
    pub(super) fn compile_export_all_declaration(
        &mut self,
        export: &ExportAllDeclaration<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        if export.export_kind.is_type() {
            return Ok(());
        }

        let specifier: Box<str> = export.source.value.as_str().into();

        if let Some(exported) = &export.exported {
            // `export * as ns from "..."`
            module.add_export(ExportRecord::ReExportNamespace {
                specifier,
                exported: exported.name().as_str().into(),
            });
        } else {
            // `export * from "..."`
            module.add_export(ExportRecord::ReExportAll { specifier });
        }

        Ok(())
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  Module export → global bridge
    // ═══════════════════════════════════════════════════════════════════════

    /// Emits `SetGlobal` for each exported local binding that isn't already
    /// stored on the global object (i.e., `const`/`let` bindings).
    /// This allows the host to read export values from the global after evaluation.
    pub(super) fn emit_module_export_globals(
        &mut self,
        exports: &[ExportRecord],
    ) -> Result<(), SourceLoweringError> {
        for export in exports {
            let local_name = match export {
                ExportRecord::Named { local, .. } => &**local,
                ExportRecord::Default { local } => &**local,
                // Re-exports don't reference local bindings.
                ExportRecord::ReExportNamed { .. }
                | ExportRecord::ReExportAll { .. }
                | ExportRecord::ReExportNamespace { .. } => continue,
            };

            // Look up the binding. If it's a local register or function binding,
            // emit SetGlobal to copy it to the global object.
            if let Ok(binding) = self.resolve_binding(local_name) {
                let register = match binding {
                    super::shared::Binding::Register(r)
                    | super::shared::Binding::Function {
                        closure_register: r,
                    } => r,
                    // Upvalues, this — skip.
                    _ => continue,
                };
                let prop = self.intern_property_name(local_name)?;
                self.instructions
                    .push(Instruction::set_global(register, prop));
            }
        }
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  Helpers
    // ═══════════════════════════════════════════════════════════════════════

    /// Declares a local variable for an import or default export binding.
    /// Returns the register allocated for it.
    fn declare_import_binding(
        &mut self,
        name: &str,
    ) -> Result<crate::bytecode::BytecodeRegister, SourceLoweringError> {
        // Import bindings are immutable locals in module scope.
        // They're populated by the host before evaluation starts.
        self.declare_variable_binding(name, false)
    }

    /// Extracts binding names from a `BindingPattern` and records them as
    /// named exports. Handles identifiers, array patterns, object patterns.
    fn collect_binding_export_names(
        &self,
        pattern: &BindingPattern<'_>,
        module: &mut ModuleCompiler<'a>,
    ) {
        match pattern {
            BindingPattern::BindingIdentifier(id) => {
                let name: Box<str> = id.name.as_str().into();
                module.add_export(ExportRecord::Named {
                    local: name.clone(),
                    exported: name,
                });
            }
            BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.collect_binding_export_names(&prop.value, module);
                }
                if let Some(rest) = &obj.rest {
                    self.collect_binding_export_names(&rest.argument, module);
                }
            }
            BindingPattern::ArrayPattern(arr) => {
                for elem in arr.elements.iter().flatten() {
                    self.collect_binding_export_names(elem, module);
                }
                if let Some(rest) = &arr.rest {
                    self.collect_binding_export_names(&rest.argument, module);
                }
            }
            BindingPattern::AssignmentPattern(assign) => {
                self.collect_binding_export_names(&assign.left, module);
            }
        }
    }
}
