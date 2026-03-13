//! AST-based dependency scanner using oxc parser.
//!
//! Extracts import/require specifiers from JavaScript/TypeScript source
//! without full compilation. Used by `ModuleGraph` for dependency discovery.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

/// A discovered dependency from source code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepRecord {
    /// The import/require specifier string (e.g., "./foo", "lodash", "node:fs")
    pub specifier: String,
    /// Whether this is a `require()` call (true) or an `import` statement/expression (false)
    pub is_require: bool,
}

/// Scan source code for import/require specifiers using the oxc AST parser.
///
/// This replaces the old regex-based `parse_imports`/`parse_requires` functions
/// with a correct AST-based approach that handles:
/// - Imports inside comments or string literals (correctly ignores them)
/// - Multi-line import statements
/// - Dynamic `import()` expressions with string literals
/// - `require()` calls with string literals
/// - `export ... from '...'` re-exports
///
/// Returns an empty vec on parse errors (best-effort).
pub fn scan_dependencies(source: &str, filename: &str) -> Vec<DepRecord> {
    let alloc = Allocator::default();
    let source_type = SourceType::from_path(filename).unwrap_or_default();

    let ret = Parser::new(&alloc, source, source_type).parse();

    if ret.panicked {
        return Vec::new();
    }

    let mut deps = Vec::new();

    // Walk top-level statements for static imports/exports
    for stmt in &ret.program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                let spec = decl.source.value.as_str().to_string();
                push_unique(&mut deps, DepRecord { specifier: spec, is_require: false });
            }
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(ref src) = decl.source {
                    let spec = src.value.as_str().to_string();
                    push_unique(&mut deps, DepRecord { specifier: spec, is_require: false });
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                let spec = decl.source.value.as_str().to_string();
                push_unique(&mut deps, DepRecord { specifier: spec, is_require: false });
            }
            _ => {}
        }
    }

    // Walk all expressions for dynamic import() and require() calls
    scan_stmts(&ret.program.body, &mut deps);

    deps
}

/// Check if source contains ESM module syntax (import/export declarations or top-level await).
///
/// Uses the oxc parser so string contents like `'import foo'` don't cause false positives.
/// This replaces the old heuristic of searching for `"import "` / `"export "` / `"await "` in
/// the raw source text.
pub fn has_module_syntax(source: &str) -> bool {
    let alloc = Allocator::default();
    // Parse as module to allow import/export syntax
    let source_type = SourceType::mjs();
    let ret = Parser::new(&alloc, source, source_type).parse();

    if ret.panicked {
        return false;
    }

    for stmt in &ret.program.body {
        match stmt {
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::ExportDefaultDeclaration(_) => return true,
            // Top-level await indicates ESM
            Statement::ExpressionStatement(expr_stmt) => {
                if matches!(&expr_stmt.expression, Expression::AwaitExpression(_)) {
                    return true;
                }
            }
            Statement::VariableDeclaration(decl) => {
                for declarator in &decl.declarations {
                    if let Some(ref init) = declarator.init {
                        if matches!(init, Expression::AwaitExpression(_)) {
                            return true;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    false
}

/// Convenience: extract just the specifier strings (for backward compat with old API).
pub fn scan_specifiers(source: &str, filename: &str) -> Vec<String> {
    scan_dependencies(source, filename)
        .into_iter()
        .map(|d| d.specifier)
        .collect()
}

fn push_unique(deps: &mut Vec<DepRecord>, record: DepRecord) {
    if !deps.iter().any(|d| d.specifier == record.specifier && d.is_require == record.is_require) {
        deps.push(record);
    }
}

fn scan_stmts(stmts: &[Statement<'_>], deps: &mut Vec<DepRecord>) {
    for stmt in stmts {
        scan_stmt(stmt, deps);
    }
}

fn scan_stmt(stmt: &Statement<'_>, deps: &mut Vec<DepRecord>) {
    match stmt {
        Statement::ExpressionStatement(expr_stmt) => {
            scan_expr(&expr_stmt.expression, deps);
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(ref init) = declarator.init {
                    scan_expr(init, deps);
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(ref arg) = ret.argument {
                scan_expr(arg, deps);
            }
        }
        Statement::IfStatement(if_stmt) => {
            scan_expr(&if_stmt.test, deps);
            scan_stmt(&if_stmt.consequent, deps);
            if let Some(ref alt) = if_stmt.alternate {
                scan_stmt(alt, deps);
            }
        }
        Statement::BlockStatement(block) => {
            scan_stmts(&block.body, deps);
        }
        Statement::ForStatement(for_stmt) => {
            if let Some(ForStatementInit::VariableDeclaration(decl)) = &for_stmt.init {
                for declarator in &decl.declarations {
                    if let Some(ref init_expr) = declarator.init {
                        scan_expr(init_expr, deps);
                    }
                }
            }
            scan_stmt(&for_stmt.body, deps);
        }
        Statement::ForInStatement(for_in) => {
            scan_expr(&for_in.right, deps);
            scan_stmt(&for_in.body, deps);
        }
        Statement::ForOfStatement(for_of) => {
            scan_expr(&for_of.right, deps);
            scan_stmt(&for_of.body, deps);
        }
        Statement::WhileStatement(while_stmt) => {
            scan_expr(&while_stmt.test, deps);
            scan_stmt(&while_stmt.body, deps);
        }
        Statement::DoWhileStatement(do_while) => {
            scan_stmt(&do_while.body, deps);
            scan_expr(&do_while.test, deps);
        }
        Statement::TryStatement(try_stmt) => {
            scan_stmts(&try_stmt.block.body, deps);
            if let Some(ref handler) = try_stmt.handler {
                scan_stmts(&handler.body.body, deps);
            }
            if let Some(ref finalizer) = try_stmt.finalizer {
                scan_stmts(&finalizer.body, deps);
            }
        }
        Statement::SwitchStatement(switch) => {
            scan_expr(&switch.discriminant, deps);
            for case in &switch.cases {
                scan_stmts(&case.consequent, deps);
            }
        }
        Statement::ExportDefaultDeclaration(decl) => {
            match &decl.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    if let Some(ref body) = func.body {
                        scan_stmts(&body.statements, deps);
                    }
                }
                ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                    for elem in &class.body.body {
                        scan_class_element(elem, deps);
                    }
                }
                _ => {
                    // Expression variant — try to extract as expression
                    if let Some(expr) = decl.declaration.as_expression() {
                        scan_expr(expr, deps);
                    }
                }
            }
        }
        Statement::FunctionDeclaration(func) => {
            if let Some(ref body) = func.body {
                scan_stmts(&body.statements, deps);
            }
        }
        Statement::ClassDeclaration(class) => {
            for elem in &class.body.body {
                scan_class_element(elem, deps);
            }
        }
        _ => {}
    }
}

fn scan_expr(expr: &Expression<'_>, deps: &mut Vec<DepRecord>) {
    match expr {
        // import('specifier')
        Expression::ImportExpression(import_expr) => {
            if let Expression::StringLiteral(lit) = &import_expr.source {
                push_unique(deps, DepRecord {
                    specifier: lit.value.as_str().to_string(),
                    is_require: false,
                });
            }
            // Also scan the source expression in case it's complex
            scan_expr(&import_expr.source, deps);
        }
        // require('specifier')
        Expression::CallExpression(call) => {
            if is_require_call(call)
                && let Some(Argument::StringLiteral(lit)) = call.arguments.first()
            {
                push_unique(deps, DepRecord {
                    specifier: lit.value.as_str().to_string(),
                    is_require: true,
                });
            }
            // Scan callee and arguments recursively
            scan_expr(&call.callee, deps);
            for arg in &call.arguments {
                scan_argument(arg, deps);
            }
        }
        Expression::AwaitExpression(await_expr) => {
            scan_expr(&await_expr.argument, deps);
        }
        Expression::AssignmentExpression(assign) => {
            scan_expr(&assign.right, deps);
        }
        Expression::SequenceExpression(seq) => {
            for e in &seq.expressions {
                scan_expr(e, deps);
            }
        }
        Expression::ConditionalExpression(cond) => {
            scan_expr(&cond.test, deps);
            scan_expr(&cond.consequent, deps);
            scan_expr(&cond.alternate, deps);
        }
        Expression::LogicalExpression(logical) => {
            scan_expr(&logical.left, deps);
            scan_expr(&logical.right, deps);
        }
        Expression::BinaryExpression(bin) => {
            scan_expr(&bin.left, deps);
            scan_expr(&bin.right, deps);
        }
        Expression::StaticMemberExpression(stat) => {
            scan_expr(&stat.object, deps);
        }
        Expression::ComputedMemberExpression(comp) => {
            scan_expr(&comp.object, deps);
            scan_expr(&comp.expression, deps);
        }
        Expression::PrivateFieldExpression(priv_field) => {
            scan_expr(&priv_field.object, deps);
        }
        Expression::ArrowFunctionExpression(arrow) => {
            scan_stmts(&arrow.body.statements, deps);
        }
        Expression::FunctionExpression(func) => {
            if let Some(ref body) = func.body {
                scan_stmts(&body.statements, deps);
            }
        }
        Expression::TemplateLiteral(tmpl) => {
            for e in &tmpl.expressions {
                scan_expr(e, deps);
            }
        }
        Expression::TaggedTemplateExpression(tagged) => {
            scan_expr(&tagged.tag, deps);
            for e in &tagged.quasi.expressions {
                scan_expr(e, deps);
            }
        }
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                match elem {
                    ArrayExpressionElement::SpreadElement(spread) => {
                        scan_expr(&spread.argument, deps);
                    }
                    ArrayExpressionElement::Elision(_) => {}
                    _ => {
                        // Other variants are expressions
                        if let Some(e) = elem.as_expression() {
                            scan_expr(e, deps);
                        }
                    }
                }
            }
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyKind::ObjectProperty(p) => {
                        scan_expr(&p.value, deps);
                    }
                    ObjectPropertyKind::SpreadProperty(spread) => {
                        scan_expr(&spread.argument, deps);
                    }
                }
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            scan_expr(&paren.expression, deps);
        }
        Expression::UnaryExpression(unary) => {
            scan_expr(&unary.argument, deps);
        }
        Expression::UpdateExpression(_) => {
            // UpdateExpression argument is SimpleAssignmentTarget, not Expression — skip
        }
        Expression::YieldExpression(yield_expr) => {
            if let Some(ref arg) = yield_expr.argument {
                scan_expr(arg, deps);
            }
        }
        Expression::ClassExpression(class) => {
            for elem in &class.body.body {
                scan_class_element(elem, deps);
            }
        }
        Expression::NewExpression(new_expr) => {
            scan_expr(&new_expr.callee, deps);
            for arg in &new_expr.arguments {
                scan_argument(arg, deps);
            }
        }
        Expression::ChainExpression(chain) => match &chain.expression {
            ChainElement::CallExpression(call) => {
                scan_expr(&call.callee, deps);
                for arg in &call.arguments {
                    scan_argument(arg, deps);
                }
            }
            ChainElement::StaticMemberExpression(stat) => {
                scan_expr(&stat.object, deps);
            }
            ChainElement::ComputedMemberExpression(comp) => {
                scan_expr(&comp.object, deps);
                scan_expr(&comp.expression, deps);
            }
            ChainElement::PrivateFieldExpression(priv_field) => {
                scan_expr(&priv_field.object, deps);
            }
            _ => {}
        },
        _ => {}
    }
}

fn scan_argument(arg: &Argument<'_>, deps: &mut Vec<DepRecord>) {
    match arg {
        Argument::SpreadElement(spread) => {
            scan_expr(&spread.argument, deps);
        }
        _ => {
            if let Some(e) = arg.as_expression() {
                scan_expr(e, deps);
            }
        }
    }
}

fn scan_class_element(elem: &ClassElement<'_>, deps: &mut Vec<DepRecord>) {
    match elem {
        ClassElement::MethodDefinition(method) => {
            if let Some(ref body) = method.value.body {
                scan_stmts(&body.statements, deps);
            }
        }
        ClassElement::PropertyDefinition(prop) => {
            if let Some(ref value) = prop.value {
                scan_expr(value, deps);
            }
        }
        ClassElement::StaticBlock(block) => {
            scan_stmts(&block.body, deps);
        }
        _ => {}
    }
}

fn is_require_call(call: &CallExpression<'_>) -> bool {
    if call.arguments.len() != 1 {
        return false;
    }
    matches!(&call.callee, Expression::Identifier(id) if id.name == "require")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_static_imports() {
        let source = r#"
            import { foo } from './foo.js';
            import bar from "https://esm.sh/bar";
            import * as utils from './utils.js';
        "#;
        let deps = scan_specifiers(source, "test.js");
        assert_eq!(deps.len(), 3);
        assert!(deps.contains(&"./foo.js".to_string()));
        assert!(deps.contains(&"https://esm.sh/bar".to_string()));
        assert!(deps.contains(&"./utils.js".to_string()));
    }

    #[test]
    fn test_dynamic_imports() {
        let source = r#"
            const mod = await import('./dynamic.js');
            import("./another.js").then(m => m.default);
        "#;
        let deps = scan_specifiers(source, "test.mjs");
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"./dynamic.js".to_string()));
        assert!(deps.contains(&"./another.js".to_string()));
    }

    #[test]
    fn test_export_from() {
        let source = r#"
            export { foo } from './foo.js';
            export * from './all.js';
        "#;
        let deps = scan_specifiers(source, "test.mjs");
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"./foo.js".to_string()));
        assert!(deps.contains(&"./all.js".to_string()));
    }

    #[test]
    fn test_require_calls() {
        let source = r#"
            const fs = require('fs');
            const path = require("path");
            const lib = require('./lib.cjs');
        "#;
        let deps = scan_dependencies(source, "test.cjs");
        assert_eq!(deps.len(), 3);
        assert!(deps.iter().all(|d| d.is_require));
        assert!(deps.iter().any(|d| d.specifier == "fs"));
        assert!(deps.iter().any(|d| d.specifier == "path"));
        assert!(deps.iter().any(|d| d.specifier == "./lib.cjs"));
    }

    #[test]
    fn test_side_effect_imports() {
        let source = r#"
            import './side-effect.js';
            import "https://esm.sh/polyfill";
        "#;
        let deps = scan_specifiers(source, "test.mjs");
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"./side-effect.js".to_string()));
        assert!(deps.contains(&"https://esm.sh/polyfill".to_string()));
    }

    #[test]
    fn test_no_duplicates() {
        let source = r#"
            import { foo } from './mod.js';
            import { bar } from './mod.js';
            const x = await import('./mod.js');
        "#;
        let deps = scan_specifiers(source, "test.mjs");
        assert_eq!(deps.len(), 1);
        assert!(deps.contains(&"./mod.js".to_string()));
    }

    #[test]
    fn test_ignores_comments_and_strings() {
        let source = r#"
            // import { foo } from './commented.js';
            /* import { bar } from './block-commented.js'; */
            const s = "import { baz } from './in-string.js'";
            import { real } from './real.js';
        "#;
        let deps = scan_specifiers(source, "test.mjs");
        assert_eq!(deps.len(), 1);
        assert!(deps.contains(&"./real.js".to_string()));
    }

    #[test]
    fn test_typescript_source() {
        let source = r#"
            import { Component } from 'react';
            import type { Props } from './types';
            const lazy = await import('./lazy.tsx');
        "#;
        let deps = scan_specifiers(source, "test.tsx");
        assert!(deps.contains(&"react".to_string()));
        assert!(deps.contains(&"./types".to_string()));
        assert!(deps.contains(&"./lazy.tsx".to_string()));
    }

    #[test]
    fn test_mixed_imports_and_requires() {
        let source = r#"
            import { foo } from './foo.js';
            const bar = require('./bar.cjs');
            export { baz } from './baz.js';
        "#;
        let deps = scan_dependencies(source, "test.js");
        assert_eq!(deps.len(), 3);
        assert!(deps.iter().any(|d| d.specifier == "./foo.js" && !d.is_require));
        assert!(deps.iter().any(|d| d.specifier == "./bar.cjs" && d.is_require));
        assert!(deps.iter().any(|d| d.specifier == "./baz.js" && !d.is_require));
    }

    #[test]
    fn test_scoped_packages() {
        let source = r#"
            const pkg = require('@scope/package');
            const sub = require('@org/lib/subpath');
        "#;
        let deps = scan_specifiers(source, "test.cjs");
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"@scope/package".to_string()));
        assert!(deps.contains(&"@org/lib/subpath".to_string()));
    }

    // has_module_syntax tests

    #[test]
    fn test_has_module_syntax_import() {
        assert!(has_module_syntax("import { foo } from './foo.js';"));
        assert!(has_module_syntax("import './side-effect.js';"));
    }

    #[test]
    fn test_has_module_syntax_export() {
        assert!(has_module_syntax("export const x = 1;"));
        assert!(has_module_syntax("export default function() {}"));
        assert!(has_module_syntax("export { foo } from './foo.js';"));
        assert!(has_module_syntax("export * from './all.js';"));
    }

    #[test]
    fn test_has_module_syntax_top_level_await() {
        assert!(has_module_syntax("const x = await fetch('http://example.com');"));
        assert!(has_module_syntax("await Promise.resolve(1);"));
    }

    #[test]
    fn test_has_module_syntax_false_for_plain_script() {
        assert!(!has_module_syntax("const x = 1;"));
        assert!(!has_module_syntax("console.log('hello');"));
        assert!(!has_module_syntax("function foo() { return 1; }"));
    }

    #[test]
    fn test_has_module_syntax_no_false_positive_from_strings() {
        // The old heuristic would match "import " inside a string
        assert!(!has_module_syntax(r#"const s = "import foo from 'bar'";"#));
        assert!(!has_module_syntax(r#"const s = "export default 42";"#));
        assert!(!has_module_syntax(r#"const s = "await something";"#));
    }

    #[test]
    fn test_has_module_syntax_await_inside_async_fn_not_toplevel() {
        // await inside an async function is NOT top-level module syntax
        assert!(!has_module_syntax("async function foo() { await bar(); }"));
    }
}
