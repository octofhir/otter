//! Annex B.3.3 block-level function declaration web-compat semantics.
//!
//! In sloppy mode, a `function f() {}` declared inside a block also
//! creates an initialized (`undefined`) `var`-style binding `f` in
//! the enclosing function / script / eval variable scope — unless
//! replacing the declaration with `var f` would produce an early
//! error (a lexical `f` anywhere between the block and the variable
//! scope, or a parameter named `f`). When the declaration's source
//! position is evaluated, the var binding is updated with the block
//! binding's current value.
//!
//! # Contents
//! - [`collect_annex_b_candidates`] — static walk producing the
//!   candidate name list for one function / script / eval body.
//!
//! # Invariants
//! - The walk never crosses into nested function bodies — their
//!   block-level declarations extend *their own* variable scope.
//! - A name blocked at any depth on the path stays blocked for all
//!   deeper blocks (the `blocked` set only grows downward).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-block-level-function-declarations-web-legacy-compatibility-semantics>

use std::collections::HashSet;

use oxc_ast::ast::Statement;

use crate::hoist::collect_lexical_var_names;

/// Collect Annex B.3.3 var-extension candidates from a function /
/// script / eval body. `blocked` carries the names that can never
/// receive the extension: top-level lexical names, parameter names,
/// and (when the arguments object is materialised) `"arguments"`.
pub(crate) fn collect_annex_b_candidates(
    stmts: &[Statement<'_>],
    blocked: &HashSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for stmt in stmts {
        walk_statement(stmt, blocked, &mut out, &mut seen);
    }
    out
}

/// Names lexically declared directly inside a statement list:
/// `let` / `const` / `class` declarations plus block-level function
/// declarations (which are themselves lexical within their block, so
/// they block deeper same-name extensions).
fn block_lexical_names(stmts: &[Statement<'_>], include_functions: bool) -> Vec<String> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for stmt in stmts {
        match stmt {
            Statement::VariableDeclaration(d)
                if matches!(
                    d.kind,
                    oxc_ast::ast::VariableDeclarationKind::Let
                        | oxc_ast::ast::VariableDeclarationKind::Const
                ) =>
            {
                collect_lexical_var_names(d, &mut out);
                // Destructured leaves are lexical names too.
                for declarator in d.declarations.iter() {
                    collect_pattern_leaf_names(&declarator.id, &mut out);
                }
            }
            Statement::ClassDeclaration(c) => {
                if let Some(id) = &c.id {
                    out.push((id.name.as_str().to_string(), false));
                }
            }
            Statement::FunctionDeclaration(f) if include_functions => {
                if let Some(id) = &f.id {
                    out.push((id.name.as_str().to_string(), false));
                }
            }
            _ => {}
        }
    }
    out.into_iter().map(|(name, _)| name).collect()
}

fn collect_pattern_leaf_names(
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    out: &mut Vec<(String, bool)>,
) {
    use oxc_ast::ast::BindingPattern;
    match pattern {
        BindingPattern::BindingIdentifier(_) => {}
        BindingPattern::AssignmentPattern(asgn) => collect_pattern_leaf_names(&asgn.left, out),
        BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_pattern_leaf_names_all(elem, out);
            }
            if let Some(rest) = &arr.rest {
                collect_pattern_leaf_names_all(&rest.argument, out);
            }
        }
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_pattern_leaf_names_all(&prop.value, out);
            }
            if let Some(rest) = &obj.rest {
                collect_pattern_leaf_names_all(&rest.argument, out);
            }
        }
    }
}

fn collect_pattern_leaf_names_all(
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    out: &mut Vec<(String, bool)>,
) {
    if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = pattern {
        out.push((id.name.as_str().to_string(), false));
    } else {
        collect_pattern_leaf_names(pattern, out);
    }
}

/// Record `f` as a candidate unless its name is blocked.
fn record_function(
    f: &oxc_ast::ast::Function<'_>,
    blocked: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    // §B.3.3 applies to plain functions only — generator / async /
    // async-generator block declarations never receive the extension.
    if f.declare || f.r#async || f.generator {
        return;
    }
    if let Some(id) = &f.id {
        let name = id.name.as_str();
        if !blocked.contains(name) && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }
}

/// Walk one statement-list scope: candidates are the block's direct
/// function declarations (minus the block's own `let`/`const`/`class`
/// blockers); deeper blocks additionally treat this block's function
/// names as lexical blockers.
fn walk_block(
    stmts: &[Statement<'_>],
    blocked: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let mut here_blocked = blocked.clone();
    here_blocked.extend(block_lexical_names(stmts, false));
    for stmt in stmts {
        if let Statement::FunctionDeclaration(f) = stmt {
            record_function(f, &here_blocked, out, seen);
        }
    }
    let mut nested_blocked = here_blocked;
    nested_blocked.extend(block_lexical_names(stmts, true));
    for stmt in stmts {
        walk_statement(stmt, &nested_blocked, out, seen);
    }
}

fn walk_statement(
    stmt: &Statement<'_>,
    blocked: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match stmt {
        Statement::BlockStatement(b) => walk_block(&b.body, blocked, out, seen),
        Statement::IfStatement(s) => {
            // §B.3.2 FunctionDeclarations in IfStatement clauses act
            // like single-statement blocks.
            walk_branch_statement(&s.consequent, blocked, out, seen);
            if let Some(alt) = &s.alternate {
                walk_branch_statement(alt, blocked, out, seen);
            }
        }
        Statement::ForStatement(s) => {
            let mut body_blocked = blocked.clone();
            if let Some(oxc_ast::ast::ForStatementInit::VariableDeclaration(d)) = &s.init
                && matches!(
                    d.kind,
                    oxc_ast::ast::VariableDeclarationKind::Let
                        | oxc_ast::ast::VariableDeclarationKind::Const
                )
            {
                extend_with_declaration_names(d, &mut body_blocked);
            }
            walk_branch_statement(&s.body, &body_blocked, out, seen);
        }
        Statement::ForInStatement(s) => {
            let mut body_blocked = blocked.clone();
            extend_with_for_head_names(&s.left, &mut body_blocked);
            walk_branch_statement(&s.body, &body_blocked, out, seen);
        }
        Statement::ForOfStatement(s) => {
            let mut body_blocked = blocked.clone();
            extend_with_for_head_names(&s.left, &mut body_blocked);
            walk_branch_statement(&s.body, &body_blocked, out, seen);
        }
        Statement::WhileStatement(s) => walk_branch_statement(&s.body, blocked, out, seen),
        Statement::DoWhileStatement(s) => walk_branch_statement(&s.body, blocked, out, seen),
        Statement::WithStatement(s) => walk_branch_statement(&s.body, blocked, out, seen),
        Statement::LabeledStatement(s) => {
            // A labelled function declaration is itself a candidate
            // (legacy LabelledFunction production).
            walk_branch_statement(&s.body, blocked, out, seen);
        }
        Statement::TryStatement(s) => {
            walk_block(&s.block.body, blocked, out, seen);
            if let Some(handler) = &s.handler {
                // The catch parameter's names block extensions from
                // inside the catch block (§B.3.4).
                let mut catch_blocked = blocked.clone();
                if let Some(param) = &handler.param {
                    let mut names: Vec<(String, bool)> = Vec::new();
                    collect_pattern_leaf_names_all(&param.pattern, &mut names);
                    catch_blocked.extend(names.into_iter().map(|(name, _)| name));
                }
                walk_block(&handler.body.body, &catch_blocked, out, seen);
            }
            if let Some(finalizer) = &s.finalizer {
                walk_block(&finalizer.body, blocked, out, seen);
            }
        }
        Statement::SwitchStatement(s) => {
            // The whole CaseBlock is one lexical scope.
            let mut here_blocked = blocked.clone();
            let mut all_fn_names: Vec<String> = Vec::new();
            for case in &s.cases {
                here_blocked.extend(block_lexical_names(&case.consequent, false));
                all_fn_names.extend(block_lexical_names(&case.consequent, true));
            }
            for case in &s.cases {
                for inner in &case.consequent {
                    if let Statement::FunctionDeclaration(f) = inner {
                        record_function(f, &here_blocked, out, seen);
                    }
                }
            }
            let mut nested_blocked = here_blocked;
            nested_blocked.extend(all_fn_names);
            for case in &s.cases {
                for inner in &case.consequent {
                    walk_statement(inner, &nested_blocked, out, seen);
                }
            }
        }
        _ => {}
    }
}

/// A single-statement branch body (if/loop/label arms): a direct
/// `FunctionDeclaration` is a candidate; anything else recurses.
fn walk_branch_statement(
    stmt: &Statement<'_>,
    blocked: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if let Statement::FunctionDeclaration(f) = stmt {
        record_function(f, blocked, out, seen);
        return;
    }
    walk_statement(stmt, blocked, out, seen);
}

/// Push every leaf name a lexical `VariableDeclaration` declares.
fn extend_with_declaration_names(
    d: &oxc_ast::ast::VariableDeclaration<'_>,
    blocked: &mut HashSet<String>,
) {
    let mut names: Vec<(String, bool)> = Vec::new();
    collect_lexical_var_names(d, &mut names);
    for declarator in d.declarations.iter() {
        collect_pattern_leaf_names(&declarator.id, &mut names);
    }
    blocked.extend(names.into_iter().map(|(name, _)| name));
}

/// §14.7.5 — a `let` / `const` for-in/of head declares per-iteration
/// lexical bindings that block the extension inside the body.
fn extend_with_for_head_names(
    left: &oxc_ast::ast::ForStatementLeft<'_>,
    blocked: &mut HashSet<String>,
) {
    if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(d) = left
        && matches!(
            d.kind,
            oxc_ast::ast::VariableDeclarationKind::Let
                | oxc_ast::ast::VariableDeclarationKind::Const
        )
    {
        extend_with_declaration_names(d, blocked);
    }
}
