//! TypeScript erasure helpers used while lowering OXC AST nodes.
//!
//! # Contents
//! - expression wrapper unwrapping
//! - erased and rejected statement classification
//! - span and node-kind helpers
//!
//! # Invariants
//! - Helpers inspect AST shape only and never rewrite source text.
//!
//! # See also
//! - `entry` for parse-to-bytecode entry points

use crate::*;

/// Strip TypeScript-only expression wrappers and parentheses,
/// returning the underlying runtime expression.
///
/// Recognises `TSAsExpression`, `TSSatisfiesExpression`,
/// `TSNonNullExpression`, `TSTypeAssertion`, and
/// `TSInstantiationExpression`. Also unwraps
/// `ParenthesizedExpression` so `(undefined as any)` and
/// `(((x as A) satisfies B)!)` collapse to their leaf expressions.
/// Recursive.
#[must_use]
pub fn unwrap_ts_expr<'a, 'b>(expr: &'a Expression<'b>) -> &'a Expression<'b> {
    match expr {
        Expression::TSAsExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSNonNullExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSTypeAssertion(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSInstantiationExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::ParenthesizedExpression(inner) => unwrap_ts_expr(&inner.expression),
        other => other,
    }
}

/// `true` for top-level TS statements that the frontend policy marks as
/// "erased" — they produce no bytecode and are not errors.
pub(crate) fn is_erased_ts_statement(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::TSTypeAliasDeclaration(_)
        | Statement::TSInterfaceDeclaration(_)
        | Statement::TSImportEqualsDeclaration(_) => true,

        // `declare function f();` and friends.
        Statement::FunctionDeclaration(f) if f.declare => true,
        Statement::ClassDeclaration(c) if c.declare => true,
        Statement::VariableDeclaration(v) if v.declare => true,

        // `import type { X } from "y"` / `import { type X } from "y"`
        // — when the whole import is type-only the declaration is
        // erased; otherwise this slice does not yet support imports.
        Statement::ImportDeclaration(d) if d.import_kind.is_type() => true,

        // `export type { ... }` / `export type X = ...`
        Statement::ExportNamedDeclaration(d) if d.export_kind.is_type() => true,
        Statement::ExportAllDeclaration(d) if d.export_kind.is_type() => true,

        // `declare module "..." { ... }` and `declare namespace N { ... }`.
        Statement::TSModuleDeclaration(m) if m.declare => true,

        _ => false,
    }
}

/// `Some((node, span))` for top-level TS statements that the frontend policy
/// marks as "diagnosed" — produce a structured `TS_UNSUPPORTED`.
pub(crate) fn rejected_ts_statement(stmt: &Statement<'_>) -> Option<(&'static str, (u32, u32))> {
    use oxc_span::GetSpan;
    match stmt {
        Statement::TSEnumDeclaration(d) => Some(("TSEnumDeclaration", (d.span.start, d.span.end))),
        // Non-`declare` namespace with a runtime body.
        Statement::TSModuleDeclaration(d) if !d.declare => {
            Some(("TSModuleDeclaration", (d.span.start, d.span.end)))
        }
        Statement::ClassDeclaration(c) if !c.decorators.is_empty() => {
            let s = c.decorators[0].span();
            Some(("Decorator", (s.start, s.end)))
        }
        _ => None,
    }
}

pub(crate) fn expr_kind_name(expr: &Expression<'_>) -> &'static str {
    use Expression::*;
    match expr {
        Identifier(_) => "Identifier",
        StringLiteral(_) => "StringLiteral",
        NumericLiteral(_) => "NumericLiteral",
        BooleanLiteral(_) => "BooleanLiteral",
        NullLiteral(_) => "NullLiteral",
        TemplateLiteral(_) => "TemplateLiteral",
        BinaryExpression(_) => "BinaryExpression",
        StaticMemberExpression(_) => "StaticMemberExpression",
        CallExpression(_) => "CallExpression",
        FunctionExpression(_) => "FunctionExpression",
        ArrayExpression(_) => "ArrayExpression",
        ObjectExpression(_) => "ObjectExpression",
        ParenthesizedExpression(_) => "ParenthesizedExpression",
        _ => "Expression",
    }
}

pub(crate) fn expr_span(expr: &Expression<'_>) -> (u32, u32) {
    use oxc_span::GetSpan;
    let s = expr.span();
    (s.start, s.end)
}

pub(crate) fn stmt_kind_name(stmt: &Statement<'_>) -> &'static str {
    match stmt {
        Statement::EmptyStatement(_) => "EmptyStatement",
        Statement::ExpressionStatement(_) => "ExpressionStatement",
        Statement::VariableDeclaration(_) => "VariableDeclaration",
        Statement::FunctionDeclaration(_) => "FunctionDeclaration",
        Statement::ClassDeclaration(_) => "ClassDeclaration",
        Statement::IfStatement(_) => "IfStatement",
        Statement::ForStatement(_) => "ForStatement",
        Statement::ForOfStatement(_) => "ForOfStatement",
        Statement::WhileStatement(_) => "WhileStatement",
        Statement::DoWhileStatement(_) => "DoWhileStatement",
        Statement::ReturnStatement(_) => "ReturnStatement",
        Statement::ThrowStatement(_) => "ThrowStatement",
        Statement::TryStatement(_) => "TryStatement",
        Statement::BlockStatement(_) => "BlockStatement",
        Statement::TSEnumDeclaration(_) => "TSEnumDeclaration",
        Statement::TSInterfaceDeclaration(_) => "TSInterfaceDeclaration",
        Statement::TSTypeAliasDeclaration(_) => "TSTypeAliasDeclaration",
        Statement::TSModuleDeclaration(_) => "TSModuleDeclaration",
        Statement::ImportDeclaration(_) => "ImportDeclaration",
        Statement::ExportNamedDeclaration(_) => "ExportNamedDeclaration",
        Statement::ExportDefaultDeclaration(_) => "ExportDefaultDeclaration",
        Statement::ExportAllDeclaration(_) => "ExportAllDeclaration",
        _ => "Statement",
    }
}

pub(crate) fn stmt_span(stmt: &Statement<'_>) -> (u32, u32) {
    use oxc_span::GetSpan;
    let s = stmt.span();
    (s.start, s.end)
}

/// Adapter for the `for(...; ...; ...)` initializer's
/// `Expression`-shaped variant. OXC's `ForStatementInit` is a
/// closed enum that mirrors `Expression`; this helper widens it
/// back to `&Expression` so the compiler can reuse `compile_expr`.
pub(crate) fn init_to_expression<'a, 'b>(
    init: &'a oxc_ast::ast::ForStatementInit<'b>,
) -> Option<&'a Expression<'b>> {
    init.as_expression()
}
