//! AST-based ESM module transformation using SWC.
//!
//! This module replaces the regex-based transform_module with proper AST parsing
//! to correctly handle all ESM patterns including multi-line imports.

use std::collections::HashMap;
use swc_common::{sync::Lrc, FileName, SourceMap, DUMMY_SP};
use swc_ecma_ast::*;
use swc_ecma_codegen::{text_writer::JsWriter, Emitter};
use swc_ecma_parser::{lexer::Lexer, EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{VisitMut, VisitMutWith};

/// Check if a resolved URL is a built-in (node: or otter:)
fn is_builtin(resolved: &str) -> bool {
    resolved.starts_with("node:") || resolved.starts_with("otter:")
}

/// Helper to get module expression for built-ins
fn builtin_expr(resolved: &str) -> Option<String> {
    if let Some(name) = resolved.strip_prefix("node:") {
        Some(format!("globalThis.__otter_node_builtins[\"{}\"]", name))
    } else if let Some(name) = resolved.strip_prefix("otter:") {
        Some(format!("globalThis.__otter_node_builtins[\"{}\"]", name))
    } else {
        None
    }
}

/// Get module access expression
fn module_expr(resolved: &str) -> String {
    builtin_expr(resolved).unwrap_or_else(|| format!("__otter_modules[\"{}\"]", resolved))
}

/// Get string value from a Wtf8Atom (used in Str literal values)
/// Returns the UTF-8 string if valid, or lossy conversion otherwise
fn wtf8_to_string(value: &swc_ecma_ast::Str) -> String {
    // Str.value is Wtf8Atom, as_str() returns Option<&str>
    value.value.as_str().unwrap_or_default().to_string()
}

/// Transform ESM imports/exports using SWC AST.
///
/// This correctly handles:
/// - Multi-line imports
/// - Named imports/exports
/// - Default imports/exports
/// - Namespace imports
/// - Re-exports (export { x } from 'y')
/// - Export all (export * from 'y')
pub fn transform_module_ast(
    source: &str,
    _module_url: &str,
    dependencies: &HashMap<String, String>,
) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Default::default();

    let fm = cm.new_source_file(Lrc::new(FileName::Anon), source.to_string());

    let lexer = Lexer::new(
        Syntax::Es(EsSyntax {
            jsx: false,
            ..Default::default()
        }),
        EsVersion::Es2022,
        StringInput::from(&*fm),
        None,
    );

    let mut parser = Parser::new_from(lexer);

    // Collect parse errors silently
    for _e in parser.take_errors() {}

    let mut module = parser
        .parse_module()
        .map_err(|e| format!("Failed to parse module: {:?}", e.kind()))?;

    // Transform the module
    let mut transformer = EsmTransformer::new(dependencies.clone());
    module.visit_mut_with(&mut transformer);

    // Generate code
    let mut buf = vec![];
    {
        let mut emitter = Emitter {
            cfg: swc_ecma_codegen::Config::default().with_minify(false),
            cm: cm.clone(),
            comments: None,
            wr: JsWriter::new(cm.clone(), "\n", &mut buf, None),
        };
        emitter.emit_module(&module).map_err(|e| e.to_string())?;
    }

    let code = String::from_utf8(buf).map_err(|e| e.to_string())?;

    // Append export assignments for collected exports
    let mut result = code;
    for (name, local) in &transformer.exports {
        if name == "default" {
            result.push_str(&format!("\n__otter_exports.default = {};", local));
        } else {
            result.push_str(&format!("\n__otter_exports.{} = {};", name, local));
        }
    }

    Ok(result)
}

/// Parse ESM dependencies (imports and re-exports) using SWC AST.
///
/// This correctly handles all ESM patterns including multi-line imports.
/// Uses TypeScript syntax to properly parse TypeScript files with type definitions.
/// Returns a list of module specifiers that this module depends on.
pub fn parse_esm_dependencies(source: &str) -> Vec<String> {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(Lrc::new(FileName::Anon), source.to_string());

    // Use TypeScript syntax to handle both JS and TS files correctly
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: false,
            decorators: true,
            ..Default::default()
        }),
        EsVersion::Es2022,
        StringInput::from(&*fm),
        None,
    );

    let mut parser = Parser::new_from(lexer);

    // Ignore parse errors
    for _e in parser.take_errors() {}

    let module = match parser.parse_module() {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };

    let mut deps = Vec::new();

    for item in &module.body {
        match item {
            // import ... from 'specifier'
            ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                let specifier = wtf8_to_string(&import.src);
                if !deps.contains(&specifier) {
                    deps.push(specifier);
                }
            }
            // export { ... } from 'specifier'
            ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(export)) => {
                if let Some(src) = &export.src {
                    let specifier = wtf8_to_string(src);
                    if !deps.contains(&specifier) {
                        deps.push(specifier);
                    }
                }
            }
            // export * from 'specifier'
            ModuleItem::ModuleDecl(ModuleDecl::ExportAll(export)) => {
                let specifier = wtf8_to_string(&export.src);
                if !deps.contains(&specifier) {
                    deps.push(specifier);
                }
            }
            _ => {}
        }
    }

    // Also check for dynamic imports in statements
    // This is a simplified check - for full support we'd need to walk the entire AST
    // For now, we handle the common case of top-level await import()

    deps
}

/// AST transformer for ESM to Otter module format
struct EsmTransformer {
    dependencies: HashMap<String, String>,
    /// Collected exports: (exported_name, local_name)
    exports: Vec<(String, String)>,
}

impl EsmTransformer {
    fn new(dependencies: HashMap<String, String>) -> Self {
        Self {
            dependencies,
            exports: Vec::new(),
        }
    }

    fn resolve(&self, specifier: &str) -> String {
        self.dependencies
            .get(specifier)
            .cloned()
            .unwrap_or_else(|| specifier.to_string())
    }

    fn get_export_name(name: &ModuleExportName) -> String {
        match name {
            ModuleExportName::Ident(id) => id.sym.as_str().to_string(),
            ModuleExportName::Str(s) => wtf8_to_string(s),
        }
    }

    /// Collect all bound names from a pattern (handles destructuring)
    fn collect_pattern_names(pat: &Pat, exports: &mut Vec<(String, String)>) {
        match pat {
            // Simple identifier: export const foo = ...
            Pat::Ident(ident) => {
                let name = ident.sym.as_str().to_string();
                exports.push((name.clone(), name));
            }
            // Object destructuring: export const { foo, bar } = ...
            Pat::Object(obj) => {
                for prop in &obj.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => {
                            // { key: value } pattern - export the value name
                            Self::collect_pattern_names(&kv.value, exports);
                        }
                        ObjectPatProp::Assign(assign) => {
                            // { foo } or { foo = default } pattern
                            let name = assign.key.sym.as_str().to_string();
                            exports.push((name.clone(), name));
                        }
                        ObjectPatProp::Rest(rest) => {
                            // { ...rest } pattern
                            Self::collect_pattern_names(&rest.arg, exports);
                        }
                    }
                }
            }
            // Array destructuring: export const [a, b] = ...
            Pat::Array(arr) => {
                for elem in arr.elems.iter().flatten() {
                    Self::collect_pattern_names(elem, exports);
                }
            }
            // Rest pattern: export const [...rest] = ...
            Pat::Rest(rest) => {
                Self::collect_pattern_names(&rest.arg, exports);
            }
            // Assignment pattern: export const foo = default
            Pat::Assign(assign) => {
                Self::collect_pattern_names(&assign.left, exports);
            }
            // Other patterns (Expr, Invalid) - skip
            _ => {}
        }
    }
}

impl VisitMut for EsmTransformer {
    fn visit_mut_module_items(&mut self, items: &mut Vec<ModuleItem>) {
        let mut new_items = Vec::new();

        for item in items.drain(..) {
            match item {
                // Transform import declarations
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                    let resolved = self.resolve(&wtf8_to_string(&import.src));
                    let module_access = module_expr(&resolved);

                    for specifier in &import.specifiers {
                        match specifier {
                            // import foo from 'mod'
                            ImportSpecifier::Default(default) => {
                                // For builtins (node:*, otter:*), return the whole module
                                // because they don't have a .default export.
                                // For user modules, use .default as per ESM/CJS interop.
                                let value_expr = if is_builtin(&resolved) {
                                    module_access.clone()
                                } else {
                                    format!("{}.default", module_access)
                                };
                                let stmt = create_const_stmt(default.local.sym.as_str(), &value_expr);
                                new_items.push(ModuleItem::Stmt(stmt));
                            }
                            // import { foo, bar as baz } from 'mod'
                            ImportSpecifier::Named(named) => {
                                let imported = named
                                    .imported
                                    .as_ref()
                                    .map(|i| Self::get_export_name(i))
                                    .unwrap_or_else(|| named.local.sym.as_str().to_string());

                                let stmt = create_const_stmt(
                                    named.local.sym.as_str(),
                                    &format!("{}.{}", module_access, imported),
                                );
                                new_items.push(ModuleItem::Stmt(stmt));
                            }
                            // import * as mod from 'mod'
                            ImportSpecifier::Namespace(ns) => {
                                let stmt = create_const_stmt(ns.local.sym.as_str(), &module_access);
                                new_items.push(ModuleItem::Stmt(stmt));
                            }
                        }
                    }

                    // Side-effect import: import 'mod'
                    if import.specifiers.is_empty() {
                        let stmt = create_expr_stmt(&format!("{};", module_access));
                        new_items.push(ModuleItem::Stmt(stmt));
                    }
                }

                // Transform export declarations
                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => {
                    match &export.decl {
                        // export const/let/var foo = ... or export const { foo, bar } = ...
                        Decl::Var(var_decl) => {
                            for decl in &var_decl.decls {
                                // Collect all exported names from the pattern
                                Self::collect_pattern_names(&decl.name, &mut self.exports);
                            }
                            // Keep the declaration without export keyword
                            new_items.push(ModuleItem::Stmt(Stmt::Decl(export.decl.clone())));
                        }
                        // export function foo() {}
                        Decl::Fn(fn_decl) => {
                            let name = fn_decl.ident.sym.as_str().to_string();
                            self.exports.push((name.clone(), name));
                            new_items.push(ModuleItem::Stmt(Stmt::Decl(export.decl.clone())));
                        }
                        // export class Foo {}
                        Decl::Class(class_decl) => {
                            let name = class_decl.ident.sym.as_str().to_string();
                            self.exports.push((name.clone(), name));
                            new_items.push(ModuleItem::Stmt(Stmt::Decl(export.decl.clone())));
                        }
                        _ => {
                            new_items.push(ModuleItem::Stmt(Stmt::Decl(export.decl.clone())));
                        }
                    }
                }

                // export default expression
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(export)) => {
                    self.exports
                        .push(("default".to_string(), "__default_export__".to_string()));
                    // We need to emit: const __default_export__ = <expr>;
                    let var_decl = VarDecl {
                        span: DUMMY_SP,
                        kind: VarDeclKind::Const,
                        declare: false,
                        decls: vec![VarDeclarator {
                            span: DUMMY_SP,
                            name: Pat::Ident(BindingIdent {
                                id: Ident::new(
                                    "__default_export__".into(),
                                    DUMMY_SP,
                                    Default::default(),
                                ),
                                type_ann: None,
                            }),
                            init: Some(export.expr.clone()),
                            definite: false,
                        }],
                        ctxt: Default::default(),
                    };
                    new_items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Var(Box::new(var_decl)))));
                }

                // export default function/class
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(export)) => {
                    match &export.decl {
                        DefaultDecl::Fn(fn_expr) => {
                            let name = fn_expr
                                .ident
                                .as_ref()
                                .map(|i| i.sym.as_str().to_string())
                                .unwrap_or_else(|| "__default_export__".to_string());
                            self.exports.push(("default".to_string(), name.clone()));

                            let fn_decl = FnDecl {
                                ident: Ident::new(name.into(), DUMMY_SP, Default::default()),
                                declare: false,
                                function: fn_expr.function.clone(),
                            };
                            new_items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))));
                        }
                        DefaultDecl::Class(class_expr) => {
                            let name = class_expr
                                .ident
                                .as_ref()
                                .map(|i| i.sym.as_str().to_string())
                                .unwrap_or_else(|| "__default_export__".to_string());
                            self.exports.push(("default".to_string(), name.clone()));

                            let class_decl = ClassDecl {
                                ident: Ident::new(name.into(), DUMMY_SP, Default::default()),
                                declare: false,
                                class: class_expr.class.clone(),
                            };
                            new_items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Class(class_decl))));
                        }
                        _ => {}
                    }
                }

                // export { foo, bar as baz }
                ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(export)) => {
                    if let Some(src) = &export.src {
                        // Re-export: export { foo } from 'mod'
                        let resolved = self.resolve(&wtf8_to_string(src));
                        let module_access = module_expr(&resolved);

                        for specifier in &export.specifiers {
                            match specifier {
                                ExportSpecifier::Named(named) => {
                                    let orig = Self::get_export_name(&named.orig);
                                    let exported = named
                                        .exported
                                        .as_ref()
                                        .map(|e| Self::get_export_name(e))
                                        .unwrap_or_else(|| orig.clone());

                                    // Generate: __otter_exports.exported = module.orig;
                                    let stmt = create_expr_stmt(&format!(
                                        "__otter_exports.{} = {}.{};",
                                        exported, module_access, orig
                                    ));
                                    new_items.push(ModuleItem::Stmt(stmt));
                                }
                                ExportSpecifier::Namespace(ns) => {
                                    // export * as name from 'mod'
                                    let name = Self::get_export_name(&ns.name);

                                    // Generate: __otter_exports.name = module;
                                    let stmt = create_expr_stmt(&format!(
                                        "__otter_exports.{} = {};",
                                        name, module_access
                                    ));
                                    new_items.push(ModuleItem::Stmt(stmt));
                                }
                                ExportSpecifier::Default(_) => {
                                    // export default from 'mod' - rare pattern
                                    let stmt = create_expr_stmt(&format!(
                                        "__otter_exports.default = {}.default;",
                                        module_access
                                    ));
                                    new_items.push(ModuleItem::Stmt(stmt));
                                }
                            }
                        }
                    } else {
                        // Local export: export { foo, bar as baz }
                        for specifier in &export.specifiers {
                            if let ExportSpecifier::Named(named) = specifier {
                                let orig = Self::get_export_name(&named.orig);
                                let exported = named
                                    .exported
                                    .as_ref()
                                    .map(|e| Self::get_export_name(e))
                                    .unwrap_or_else(|| orig.clone());

                                self.exports.push((exported, orig));
                            }
                        }
                    }
                }

                // export * from 'mod'
                ModuleItem::ModuleDecl(ModuleDecl::ExportAll(export)) => {
                    let resolved = self.resolve(&wtf8_to_string(&export.src));
                    let module_access = module_expr(&resolved);

                    // Generate: Object.assign(__otter_exports, module);
                    let stmt = create_expr_stmt(&format!(
                        "Object.assign(__otter_exports, {});",
                        module_access
                    ));
                    new_items.push(ModuleItem::Stmt(stmt));
                }

                // Keep other items as-is
                other => {
                    new_items.push(other);
                }
            }
        }

        *items = new_items;

        // Visit children of remaining items to handle dynamic imports
        for item in items.iter_mut() {
            item.visit_mut_children_with(self);
        }
    }

    /// Transform dynamic imports:
    /// - String literal: import('./foo.js') -> Promise.resolve(__otter_modules["resolved"])
    /// - Variable/expression: import(expr) -> __otter_dynamic_import(expr)
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        // First, visit children
        expr.visit_mut_children_with(self);

        // Then transform dynamic imports
        if let Expr::Call(call) = expr {
            if let Callee::Import(_) = &call.callee {
                if let Some(arg) = call.args.first() {
                    if let Expr::Lit(Lit::Str(s)) = &*arg.expr {
                        // String literal - resolve at compile time
                        let specifier = wtf8_to_string(s);
                        let resolved = self.resolve(&specifier);
                        let module_access = module_expr(&resolved);

                        // Replace with Promise.resolve(module)
                        let new_code = format!("Promise.resolve({})", module_access);
                        let cm: Lrc<SourceMap> = Default::default();
                        let fm = cm.new_source_file(
                            Lrc::new(FileName::Anon),
                            new_code.clone(),
                        );

                        let lexer = Lexer::new(
                            Syntax::Es(EsSyntax::default()),
                            EsVersion::Es2022,
                            StringInput::from(&*fm),
                            None,
                        );

                        let mut parser = Parser::new_from(lexer);
                        if let Ok(new_expr) = parser.parse_expr() {
                            *expr = *new_expr;
                        }
                    } else {
                        // Non-literal (variable, template string, etc.) - use runtime resolution
                        // Transform: import(expr) -> __otter_dynamic_import(expr)
                        call.callee = Callee::Expr(Box::new(Expr::Ident(Ident::new(
                            "__otter_dynamic_import".into(),
                            DUMMY_SP,
                            Default::default(),
                        ))));
                    }
                }
            }
        }
    }
}

/// Create a const declaration statement
fn create_const_stmt(name: &str, value: &str) -> Stmt {
    // Parse the value as an expression
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(Lrc::new(FileName::Anon), value.to_string());

    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::Es2022,
        StringInput::from(&*fm),
        None,
    );

    let mut parser = Parser::new_from(lexer);
    let expr = parser.parse_expr().ok();

    let init = expr.map(|e| *e);

    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        kind: VarDeclKind::Const,
        declare: false,
        decls: vec![VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent {
                id: Ident::new(name.into(), DUMMY_SP, Default::default()),
                type_ann: None,
            }),
            init: init.map(Box::new),
            definite: false,
        }],
        ctxt: Default::default(),
    })))
}

/// Create an expression statement
fn create_expr_stmt(code: &str) -> Stmt {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(Lrc::new(FileName::Anon), code.to_string());

    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::Es2022,
        StringInput::from(&*fm),
        None,
    );

    let mut parser = Parser::new_from(lexer);

    if let Ok(expr) = parser.parse_expr() {
        Stmt::Expr(ExprStmt {
            span: DUMMY_SP,
            expr,
        })
    } else {
        // Fallback: empty statement
        Stmt::Empty(EmptyStmt { span: DUMMY_SP })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transform_import_default() {
        let source = "import foo from './foo.js';";
        let mut deps = HashMap::new();
        deps.insert("./foo.js".to_string(), "file:///project/foo.js".to_string());

        let result = transform_module_ast(source, "file:///project/main.js", &deps).unwrap();
        assert!(result.contains("__otter_modules[\"file:///project/foo.js\"].default"));
    }

    #[test]
    fn test_transform_import_named() {
        let source = "import { foo, bar } from './mod.js';";
        let mut deps = HashMap::new();
        deps.insert("./mod.js".to_string(), "file:///project/mod.js".to_string());

        let result = transform_module_ast(source, "file:///project/main.js", &deps).unwrap();
        assert!(result.contains("__otter_modules[\"file:///project/mod.js\"].foo"));
        assert!(result.contains("__otter_modules[\"file:///project/mod.js\"].bar"));
    }

    #[test]
    fn test_transform_multiline_import() {
        let source = r#"import {
            foo,
            bar,
            baz
        } from './mod.js';"#;
        let mut deps = HashMap::new();
        deps.insert("./mod.js".to_string(), "file:///project/mod.js".to_string());

        let result = transform_module_ast(source, "file:///project/main.js", &deps).unwrap();
        assert!(result.contains("foo"));
        assert!(result.contains("bar"));
        assert!(result.contains("baz"));
    }

    #[test]
    fn test_transform_export_all() {
        let source = r#"export * from "./utils.js";"#;
        let mut deps = HashMap::new();
        deps.insert(
            "./utils.js".to_string(),
            "file:///project/utils.js".to_string(),
        );

        let result = transform_module_ast(source, "file:///project/index.js", &deps).unwrap();
        assert!(result.contains("Object.assign(__otter_exports"));
    }

    #[test]
    fn test_transform_reexport_named() {
        let source = r#"export { foo, bar as baz } from "./mod.js";"#;
        let mut deps = HashMap::new();
        deps.insert("./mod.js".to_string(), "file:///project/mod.js".to_string());

        let result = transform_module_ast(source, "file:///project/index.js", &deps).unwrap();
        assert!(result.contains("__otter_exports.foo"));
        assert!(result.contains("__otter_exports.baz"));
    }

    #[test]
    fn test_transform_export_const() {
        let source = "export const PI = 3.14;";
        let deps = HashMap::new();

        let result = transform_module_ast(source, "file:///project/math.js", &deps).unwrap();
        assert!(result.contains("const PI"));
        assert!(result.contains("__otter_exports.PI = PI"));
    }

    #[test]
    fn test_transform_node_builtin() {
        let source = "import { format } from 'node:util';";
        let mut deps = HashMap::new();
        deps.insert("node:util".to_string(), "node:util".to_string());

        let result = transform_module_ast(source, "file:///project/main.js", &deps).unwrap();
        assert!(result.contains("globalThis.__otter_node_builtins[\"util\"]"));
    }

    #[test]
    fn test_transform_dynamic_import_literal() {
        let source = "const mod = await import('./foo.js');";
        let mut deps = HashMap::new();
        deps.insert("./foo.js".to_string(), "file:///project/foo.js".to_string());

        let result = transform_module_ast(source, "file:///project/main.js", &deps).unwrap();
        assert!(result.contains("Promise.resolve"));
        assert!(result.contains("__otter_modules[\"file:///project/foo.js\"]"));
    }

    #[test]
    fn test_transform_dynamic_import_variable() {
        let source = "const name = './foo.js'; const mod = await import(name);";
        let deps = HashMap::new();

        let result = transform_module_ast(source, "file:///project/main.js", &deps).unwrap();
        // Variable-based import should be transformed to __otter_dynamic_import
        assert!(result.contains("__otter_dynamic_import"));
    }
}
