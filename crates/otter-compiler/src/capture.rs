//! Capture analysis: which of a function's own bindings are read /
//! written from inside a nested function?
//!
//! Closure semantics need every binding that escapes its lexical
//! owner to live in a heap-shared
//! [`UpvalueCell`](otter_vm::UpvalueCell), not a register slot. This
//! module performs a **single pre-pass** over each function body
//! that returns the set of names which need that promotion.
//!
//! # Contents
//! - [`analyze_function`] — pre-pass driver for one function /
//!   arrow / module body.
//!
//! # Invariants
//! - The pre-pass is conservative: it ignores shadowing inside
//!   nested functions, so a nested `let n` that shadows the outer
//!   `n` may still mark the outer `n` as captured. That is harmless
//!   (one extra cell allocated) and lets us skip per-scope shadow
//!   tracking inside this analyzer.
//! - We never recurse across module / file boundaries — each
//!   function body is its own analysis unit.

use std::collections::HashSet;

use oxc_ast::ast::{
    ArrowFunctionExpression, BindingPattern, Class, FormalParameters, Function, FunctionBody,
    Statement,
};
use oxc_ast_visit::{Visit, walk};

/// Names declared by a function that some inner / nested function
/// references. The compiler turns each of these into a fresh
/// [`UpvalueCell`](otter_vm::UpvalueCell) at frame creation time.
#[must_use]
pub fn analyze_function(
    params: Option<&FormalParameters<'_>>,
    body: &FunctionBody<'_>,
) -> HashSet<String> {
    let mut own = OwnNameCollector::default();
    if let Some(p) = params {
        own.visit_formal_parameters(p);
    }
    own.names.insert("arguments".to_string());
    own.visit_function_body(body);

    let mut inner = InnerRefCollector::default();
    inner.visit_function_body(body);

    own.names.intersection(&inner.refs).cloned().collect()
}

/// Same as [`analyze_function`] but for arrow bodies (which carry a
/// [`FunctionBody`] but no separate name).
#[must_use]
pub fn analyze_arrow(arrow: &ArrowFunctionExpression<'_>) -> HashSet<String> {
    let mut own = OwnNameCollector::default();
    own.visit_formal_parameters(&arrow.params);
    own.visit_function_body(&arrow.body);

    let mut inner = InnerRefCollector::default();
    inner.visit_function_body(&arrow.body);

    own.names.intersection(&inner.refs).cloned().collect()
}

/// `true` when functions nested inside `params` / `body` reference
/// `name`. Used to decide whether a function expression's self-name
/// binding (§10.2.11 funcEnv) must live in an upvalue cell so those
/// nested closures can capture it.
#[must_use]
pub fn inner_references_name(
    params: Option<&FormalParameters<'_>>,
    body: &FunctionBody<'_>,
    name: &str,
) -> bool {
    let mut inner = InnerRefCollector::default();
    if let Some(p) = params {
        inner.visit_formal_parameters(p);
    }
    inner.visit_function_body(body);
    inner.refs.contains(name)
}

/// `true` when a function body contains a direct-eval call site —
/// a bare `eval(...)` identifier call — at any nesting depth.
/// §19.2.1.3 EvalDeclarationInstantiation gives such an eval body
/// read/write access to the caller's variable environment, so every
/// function-scope binding must live in an [`UpvalueCell`] the
/// runtime can hand to the eval chunk. The check is conservative:
/// a locally shadowed `eval` still trips it (one extra cell per
/// binding, no semantic change).
#[must_use]
pub fn body_contains_direct_eval(
    params: Option<&FormalParameters<'_>>,
    body: &FunctionBody<'_>,
) -> bool {
    let mut finder = DirectEvalFinder::default();
    if let Some(p) = params {
        finder.visit_formal_parameters(p);
    }
    finder.visit_function_body(body);
    finder.found
}

/// All names a function body declares at its own depth (parameters,
/// `var` / `let` / `const` / function / class declarations, excluding
/// nested function internals). Used to promote *every* function-scope
/// binding to an upvalue cell when the body contains a direct eval.
#[must_use]
pub fn all_own_names(
    params: Option<&FormalParameters<'_>>,
    body: &FunctionBody<'_>,
) -> HashSet<String> {
    let mut own = OwnNameCollector::default();
    if let Some(p) = params {
        own.visit_formal_parameters(p);
    }
    own.names.insert("arguments".to_string());
    own.visit_function_body(body);
    own.names
}

/// Expression variant of [`body_contains_direct_eval`] — used for
/// class field initializers, which compile into the synthesized
/// constructor's frame.
#[must_use]
pub fn expression_contains_direct_eval(expr: &oxc_ast::ast::Expression<'_>) -> bool {
    let mut finder = DirectEvalFinder::default();
    finder.visit_expression(expr);
    finder.found
}

/// Statement-list variant of [`body_contains_direct_eval`] for
/// script / eval program bodies.
#[must_use]
pub fn program_contains_direct_eval(stmts: &[Statement<'_>]) -> bool {
    let mut finder = DirectEvalFinder::default();
    for stmt in stmts {
        finder.visit_statement(stmt);
    }
    finder.found
}

/// Statement-list variant of [`all_own_names`] for script / eval
/// program bodies.
#[must_use]
pub fn all_program_names(stmts: &[Statement<'_>]) -> HashSet<String> {
    let mut own = OwnNameCollector::default();
    for stmt in stmts {
        own.visit_statement(stmt);
    }
    own.names
}

/// `true` when a program body references `new.target` outside any
/// non-arrow function. §19.2.1.1 PerformEval step 5 — such a
/// reference is an early SyntaxError unless the eval is a direct
/// eval contained in function code (arrows are transparent: they
/// inherit `new.target` lexically).
#[must_use]
pub fn program_references_new_target(stmts: &[Statement<'_>]) -> bool {
    #[derive(Default)]
    struct NewTargetFinder {
        found: bool,
    }
    impl<'a> Visit<'a> for NewTargetFinder {
        fn visit_meta_property(&mut self, it: &oxc_ast::ast::MetaProperty<'a>) {
            if it.meta.name == "new" && it.property.name == "target" {
                self.found = true;
            }
        }
        fn visit_function(&mut self, _it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {
            // Non-arrow function bodies own their `new.target`.
        }
    }
    let mut finder = NewTargetFinder::default();
    for stmt in stmts {
        finder.visit_statement(stmt);
    }
    finder.found
}

#[derive(Default)]
struct DirectEvalFinder {
    found: bool,
}

/// §13.3.6.1 — a callee that is the bare `eval` identifier, possibly
/// wrapped in parentheses (`(eval)`, `((eval))`), is a direct eval. A
/// non-trivial parenthesized callee such as `(1, eval)` is indirect.
fn callee_is_direct_eval(callee: &oxc_ast::ast::Expression<'_>) -> bool {
    match callee {
        oxc_ast::ast::Expression::Identifier(id) => id.name.as_str() == "eval",
        oxc_ast::ast::Expression::ParenthesizedExpression(p) => {
            callee_is_direct_eval(&p.expression)
        }
        _ => false,
    }
}

impl<'a> Visit<'a> for DirectEvalFinder {
    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        if callee_is_direct_eval(&it.callee) {
            self.found = true;
            return;
        }
        walk::walk_call_expression(self, it);
    }
}

/// Module-body variant: collect names declared at the top level of
/// `<main>` that some nested function references.
#[must_use]
pub fn analyze_module(stmts: &[Statement<'_>]) -> HashSet<String> {
    let mut own = OwnNameCollector::default();
    for stmt in stmts {
        own.visit_statement(stmt);
    }
    let mut inner = InnerRefCollector::default();
    for stmt in stmts {
        inner.visit_statement(stmt);
    }
    own.names.intersection(&inner.refs).cloned().collect()
}

/// Names referenced from nested functions contained in `stmts`.
///
/// Used for block-scope predeclaration. Function-wide capture analysis is
/// intentionally name-only, but a later closure that captures an outer `x`
/// must not force an unrelated earlier `{ const x }` block binding into an
/// upvalue cell.
#[must_use]
pub fn nested_function_refs_in_statements(stmts: &[Statement<'_>]) -> HashSet<String> {
    let mut inner = InnerRefCollector::default();
    for stmt in stmts {
        inner.visit_statement(stmt);
    }
    inner.refs
}

#[must_use]
pub fn nested_function_refs_in_statement_refs(stmts: &[&Statement<'_>]) -> HashSet<String> {
    let mut inner = InnerRefCollector::default();
    for stmt in stmts {
        inner.visit_statement(stmt);
    }
    inner.refs
}

/// Walks a function body and collects names declared in it (params,
/// `let` / `const` / function declarations at any block depth),
/// excluding anything declared inside a nested function.
#[derive(Default)]
struct OwnNameCollector {
    names: HashSet<String>,
    nested_depth: u32,
}

impl OwnNameCollector {
    fn maybe_collect_pattern(&mut self, pattern: &BindingPattern<'_>) {
        if self.nested_depth > 0 {
            return;
        }
        self.collect_pattern_leaves(pattern);
    }

    /// Collect every leaf identifier a binding pattern declares —
    /// `let { a, b: [c, ...d] } = …` declares `a`, `c`, `d` — so a
    /// nested function capturing a destructured leaf promotes it to
    /// an upvalue cell just like a plain `let` binding.
    fn collect_pattern_leaves(&mut self, pattern: &BindingPattern<'_>) {
        match pattern {
            BindingPattern::BindingIdentifier(id) => {
                self.names.insert(id.name.as_str().to_string());
            }
            BindingPattern::AssignmentPattern(asgn) => {
                self.collect_pattern_leaves(&asgn.left);
            }
            BindingPattern::ArrayPattern(arr) => {
                for elem in arr.elements.iter().flatten() {
                    self.collect_pattern_leaves(elem);
                }
                if let Some(rest) = &arr.rest {
                    self.collect_pattern_leaves(&rest.argument);
                }
            }
            BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.collect_pattern_leaves(&prop.value);
                }
                if let Some(rest) = &obj.rest {
                    self.collect_pattern_leaves(&rest.argument);
                }
            }
        }
    }
}

impl<'a> Visit<'a> for OwnNameCollector {
    fn visit_function(&mut self, it: &Function<'a>, flags: oxc_syntax::scope::ScopeFlags) {
        // Function declarations binding their own id at the parent
        // scope happen here (when this is a declaration, not an
        // expression).
        if self.nested_depth == 0
            && let Some(id) = it.id.as_ref()
        {
            self.names.insert(id.name.as_str().to_string());
        }
        self.nested_depth = self.nested_depth.saturating_add(1);
        walk::walk_function(self, it, flags);
        self.nested_depth = self.nested_depth.saturating_sub(1);
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.nested_depth = self.nested_depth.saturating_add(1);
        walk::walk_arrow_function_expression(self, it);
        self.nested_depth = self.nested_depth.saturating_sub(1);
    }

    fn visit_formal_parameters(&mut self, it: &FormalParameters<'a>) {
        // The rest element (`function f(...args)`) lives in `FormalParameters.rest`,
        // not in `items`, so `visit_formal_parameter` never sees it. Collect its
        // leaves explicitly, otherwise a rest parameter referenced from a nested
        // closure is not promoted to an upvalue cell and resolves as undefined.
        if let Some(rest) = &it.rest {
            self.maybe_collect_pattern(&rest.rest.argument);
        }
        walk::walk_formal_parameters(self, it);
    }

    fn visit_formal_parameter(&mut self, it: &oxc_ast::ast::FormalParameter<'a>) {
        self.maybe_collect_pattern(&it.pattern);
        walk::walk_formal_parameter(self, it);
    }

    fn visit_variable_declarator(&mut self, it: &oxc_ast::ast::VariableDeclarator<'a>) {
        self.maybe_collect_pattern(&it.id);
        walk::walk_variable_declarator(self, it);
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        // Class declarations (and named class expressions) bind
        // the class name in the enclosing scope, just like function
        // declarations. Without this hook the capture analyser
        // would miss class names referenced from inside methods.
        if self.nested_depth == 0
            && let Some(id) = it.id.as_ref()
        {
            self.names.insert(id.name.as_str().to_string());
        }
        self.nested_depth = self.nested_depth.saturating_add(1);
        walk::walk_class(self, it);
        self.nested_depth = self.nested_depth.saturating_sub(1);
    }
}

/// Walks a function body and collects every identifier name
/// referenced from inside any nested function (transitively).
#[derive(Default)]
struct InnerRefCollector {
    refs: HashSet<String>,
    nested_depth: u32,
}

impl<'a> Visit<'a> for InnerRefCollector {
    fn visit_function(&mut self, it: &Function<'a>, flags: oxc_syntax::scope::ScopeFlags) {
        self.nested_depth = self.nested_depth.saturating_add(1);
        walk::walk_function(self, it, flags);
        self.nested_depth = self.nested_depth.saturating_sub(1);
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.nested_depth = self.nested_depth.saturating_add(1);
        walk::walk_arrow_function_expression(self, it);
        self.nested_depth = self.nested_depth.saturating_sub(1);
    }

    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        if self.nested_depth > 0 {
            self.refs.insert(it.name.as_str().to_string());
        }
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        // Class methods sit inside a Function value, so the
        // function visit hooks already increment nested_depth for
        // bodies. The class header itself (super_class expression)
        // is at the current scope's depth — leave it untouched so
        // `class B extends A {}` doesn't spuriously mark `A` as a
        // captured-by-inner reference at module top level.
        walk::walk_class(self, it);
    }

    fn visit_property_definition(&mut self, it: &oxc_ast::ast::PropertyDefinition<'a>) {
        // Field initialisers (`class C { x = expr }`) are emitted by
        // the compiler inside a synthesised function frame: instance
        // fields run inside the constructor, static fields run via
        // `Op::CallWithThis` against the class's statics object.
        // Treat the value expression as if it were nested so any
        // outer-scope identifier it references is marked as captured.
        // Computed property keys (`class C { [expr] = … }`) likewise
        // currently lower into the synthesised constructor frame, so
        // their identifier references must escape the surrounding
        // scope too.
        if it.computed
            && let Some(key) = it.key.as_expression()
        {
            self.nested_depth = self.nested_depth.saturating_add(1);
            self.visit_expression(key);
            self.nested_depth = self.nested_depth.saturating_sub(1);
        }
        if let Some(value) = &it.value {
            self.nested_depth = self.nested_depth.saturating_add(1);
            self.visit_expression(value);
            self.nested_depth = self.nested_depth.saturating_sub(1);
        }
    }

    fn visit_static_block(&mut self, it: &oxc_ast::ast::StaticBlock<'a>) {
        // §15.7.4 — a static block compiles into a synthesised
        // parameterless function called via `Op::CallWithThis`.
        self.nested_depth = self.nested_depth.saturating_add(1);
        walk::walk_static_block(self, it);
        self.nested_depth = self.nested_depth.saturating_sub(1);
    }
}
