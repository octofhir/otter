//! Capture analysis: which of a function's own bindings must live in a
//! context slot rather than a register.
//!
//! A binding needs a slot when a nested function references it, when any
//! direct eval (strict or sloppy) can see it, when a sloppy mapped arguments
//! object aliases it, or — for a derived constructor's `this` — when an
//! arrow, a direct eval, or an arrow `super()` observes it. This module runs
//! name-keyed pre-passes over one function body at a time; over-promotion is
//! sound (a slot nothing reads), under-promotion is not.
//!
//! # Contents
//! - [`analyze_function`] / [`analyze_module`] — names a
//!   nested function references (every own name when a direct eval sits
//!   anywhere inside).
//! - [`body_contains_direct_eval`] and friends — direct-eval detection at any
//!   depth, and [`own_code_contains_direct_eval`] for the function's own
//!   code only (the eval-extension anchor).
//! - [`derived_this_observed`] — whether a derived constructor keeps `this`
//!   in a `DerivedThis` slot.
//! - [`statement_contains_closure_or_eval`] / [`expression_nested_refs`] —
//!   per-construct refinements for `with` objects and loop-head TDZ scopes.
//!
//! # Invariants
//! - Shadowing is respected: a nested function that binds its own
//!   parameter or local of the same spelling as an outer name does
//!   not mark that outer name as captured.
//! - We never recurse across module / file boundaries — each
//!   function body is its own analysis unit.

use std::collections::HashSet;

use oxc_ast::ast::{
    ArrowFunctionExpression, BindingPattern, Class, FormalParameters, Function, FunctionBody,
    Statement,
};
use oxc_ast_visit::{Visit, walk};

/// Names declared by a function that some inner / nested function
/// references, or every own name when a direct eval sits anywhere inside.
/// The compiler keeps each such binding in a context slot.
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

    // Closures in parameter initializers capture the parameter scope too.
    let mut inner = InnerRefCollector::default();
    if let Some(p) = params {
        inner.visit_formal_parameters(p);
    }
    inner.visit_function_body(body);

    if inner.nested_direct_eval {
        // §19.2.1.3 — a direct eval inside a nested function can read any
        // visible outer binding; promote every own name to a slot.
        return own.names;
    }
    own.names.intersection(&inner.refs).cloned().collect()
}

/// `true` when functions nested inside `params` / `body` reference
/// `name`. Used to decide whether a function expression's self-name
/// binding (§10.2.11 funcEnv) must live in a context slot.
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

/// `true` when a function body (or its parameter defaults) references
/// `name` as an identifier *at any depth* — directly in the body
/// (`return f(n - 1)`) or inside a nested closure. Unlike
/// [`inner_references_name`], which counts only nested references for
/// slot promotion, this asks whether the function's self-name is
/// observable at all, which is what decides whether the §10.2.11
/// self-binding (`LoadSelf`) is live or dead code. Conservative: a
/// nested binding that shadows `name` still trips it (the worst case
/// is emitting an unobservable self-binding, never dropping a live
/// one).
#[must_use]
pub fn body_references_name(
    params: Option<&FormalParameters<'_>>,
    body: &FunctionBody<'_>,
    name: &str,
) -> bool {
    let mut any = AnyRefCollector::default();
    if let Some(p) = params {
        any.visit_formal_parameters(p);
    }
    any.visit_function_body(body);
    any.refs.contains(name)
}

/// `true` when a function body contains a direct-eval call site —
/// a bare `eval(...)` identifier call — at any nesting depth.
/// §19.2.1.3 EvalDeclarationInstantiation gives such an eval body
/// read/write access to the caller's scope chain, so every binding it can
/// see must live in a context slot. The check is conservative: a locally
/// shadowed `eval` still trips it (one extra slot per binding, no semantic
/// change).
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
/// binding to a context slot when the body contains a direct eval.
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

/// `true` when the function's OWN code — parameters and body, excluding
/// nested functions, arrows, and class bodies (which own their variable
/// environments or are strict) — contains a direct-eval call. Such a sloppy
/// function's variable scope anchors the eval's `var` extension.
#[must_use]
pub fn own_code_contains_direct_eval(
    params: Option<&FormalParameters<'_>>,
    body: Option<&FunctionBody<'_>>,
) -> bool {
    let mut finder = OwnEvalFinder::default();
    if let Some(p) = params {
        finder.visit_formal_parameters(p);
    }
    if let Some(body) = body {
        finder.visit_function_body(body);
    }
    finder.found
}

/// [`own_code_contains_direct_eval`] restricted to the parameter list:
/// an eval in a parameter initializer declares its `var`s in the callee
/// environment outside the parameters (§10.2.11 step 20).
#[must_use]
pub fn params_contain_direct_eval(params: &FormalParameters<'_>) -> bool {
    let mut finder = OwnEvalFinder::default();
    finder.visit_formal_parameters(params);
    finder.found
}

#[derive(Default)]
struct OwnEvalFinder {
    found: bool,
}

impl<'a> Visit<'a> for OwnEvalFinder {
    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        if callee_is_direct_eval(&it.callee) {
            self.found = true;
            return;
        }
        walk::walk_call_expression(self, it);
    }
    fn visit_function(&mut self, _it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'a>) {}
    fn visit_class(&mut self, _it: &Class<'a>) {}
}

/// `true` when a derived constructor's `this` must live in a
/// `DerivedThis` context slot: an arrow nested in its parameters or body
/// (through arrows only) reads `this`, calls `super(...)`, or reads a
/// `super` property, or the constructor's own code (arrows included)
/// contains a direct eval that may do any of these.
#[must_use]
pub fn derived_this_observed(
    params: &FormalParameters<'_>,
    body: Option<&FunctionBody<'_>>,
) -> bool {
    let mut finder = DerivedThisFinder::default();
    finder.visit_formal_parameters(params);
    if let Some(body) = body {
        finder.visit_function_body(body);
    }
    finder.found
}

#[derive(Default)]
struct DerivedThisFinder {
    arrow_depth: u32,
    found: bool,
}

impl<'a> Visit<'a> for DerivedThisFinder {
    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        if callee_is_direct_eval(&it.callee) {
            self.found = true;
            return;
        }
        walk::walk_call_expression(self, it);
    }
    fn visit_this_expression(&mut self, _it: &oxc_ast::ast::ThisExpression) {
        if self.arrow_depth > 0 {
            self.found = true;
        }
    }
    fn visit_super(&mut self, _it: &oxc_ast::ast::Super) {
        if self.arrow_depth > 0 {
            self.found = true;
        }
    }
    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.arrow_depth += 1;
        walk::walk_arrow_function_expression(self, it);
        self.arrow_depth -= 1;
    }
    // Ordinary functions own `this`.
    fn visit_function(&mut self, _it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {}
    fn visit_class(&mut self, it: &Class<'a>) {
        // Heritage and computed keys evaluate in the surrounding code;
        // member bodies own their `this`.
        if let Some(super_class) = &it.super_class {
            self.visit_expression(super_class);
        }
        for element in &it.body.body {
            let key = match element {
                oxc_ast::ast::ClassElement::MethodDefinition(m) if m.computed => Some(&m.key),
                oxc_ast::ast::ClassElement::PropertyDefinition(p) if p.computed => Some(&p.key),
                oxc_ast::ast::ClassElement::AccessorProperty(a) if a.computed => Some(&a.key),
                _ => None,
            };
            if let Some(key) = key {
                self.visit_property_key(key);
            }
        }
    }
}

/// `true` when `stmt` contains a function, arrow, class, or direct eval —
/// any construct that can reach a `with` object after the statement's own
/// straight-line code, so the object must live in a context slot.
#[must_use]
pub fn statement_contains_closure_or_eval(stmt: &Statement<'_>) -> bool {
    #[derive(Default)]
    struct Finder {
        found: bool,
    }
    impl<'a> Visit<'a> for Finder {
        fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
            if callee_is_direct_eval(&it.callee) {
                self.found = true;
                return;
            }
            walk::walk_call_expression(self, it);
        }
        fn visit_function(&mut self, _it: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {
            self.found = true;
        }
        fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'a>) {
            self.found = true;
        }
        fn visit_class(&mut self, _it: &Class<'a>) {
            self.found = true;
        }
    }
    let mut finder = Finder::default();
    finder.visit_statement(stmt);
    finder.found
}

/// Names referenced from functions nested inside `expr`, and whether it
/// contains a direct eval. Decides which `for-in` / `for-of` head names a
/// closure in the right-hand side can observe in their TDZ.
#[must_use]
pub fn expression_nested_refs(expr: &oxc_ast::ast::Expression<'_>) -> (HashSet<String>, bool) {
    let mut inner = InnerRefCollector::default();
    inner.visit_expression(expr);
    (inner.refs, inner.nested_direct_eval)
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

/// `true` when some expression in `stmts` reads a `.arguments` property.
///
/// The legacy `fn.arguments` accessor (§B.3.6.2) hands back the arguments
/// object of a sloppy function's running activation, which cannot be seen
/// from the callee's own body. A whole-program scan is the cheapest sound
/// trigger: a script that never names the property pays nothing, and one
/// that does materializes the object in its sloppy functions.
#[must_use]
pub fn program_reads_dot_arguments(stmts: &[Statement<'_>]) -> bool {
    let mut finder = DotArgumentsFinder::default();
    for stmt in stmts {
        finder.visit_statement(stmt);
        if finder.found {
            return true;
        }
    }
    finder.found
}

/// `true` when the body of one function, including its arrow functions but
/// not its other nested functions, reads a `.arguments` property.
///
/// Like SpiderMonkey, only such a function captures its actual arguments at
/// call time: `fn.arguments` is almost always read inside `fn` itself. Any
/// other running activation answers the accessor from its live parameter
/// registers, so an unrelated `node.arguments` elsewhere in the unit does
/// not pessimize every function.
#[must_use]
pub fn function_body_reads_dot_arguments(
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &oxc_ast::ast::FunctionBody<'_>,
) -> bool {
    let mut finder = DotArgumentsFinder {
        found: false,
        skip_nested_functions: true,
    };
    finder.visit_formal_parameters(params);
    if !finder.found {
        finder.visit_function_body(body);
    }
    finder.found
}

#[derive(Default)]
struct DotArgumentsFinder {
    found: bool,
    /// Stop at nested non-arrow functions, which own their activations.
    skip_nested_functions: bool,
}

impl<'a> Visit<'a> for DotArgumentsFinder {
    fn visit_function(
        &mut self,
        it: &oxc_ast::ast::Function<'a>,
        flags: oxc_syntax::scope::ScopeFlags,
    ) {
        if !self.skip_nested_functions {
            walk::walk_function(self, it, flags);
        }
    }

    fn visit_class(&mut self, it: &oxc_ast::ast::Class<'a>) {
        if !self.skip_nested_functions {
            walk::walk_class(self, it);
        }
    }

    fn visit_static_member_expression(&mut self, it: &oxc_ast::ast::StaticMemberExpression<'a>) {
        if it.property.name.as_str() == "arguments" {
            self.found = true;
            return;
        }
        walk::walk_static_member_expression(self, it);
    }

    fn visit_computed_member_expression(
        &mut self,
        it: &oxc_ast::ast::ComputedMemberExpression<'a>,
    ) {
        if matches!(
            &it.expression,
            oxc_ast::ast::Expression::StringLiteral(key) if key.value.as_str() == "arguments"
        ) {
            self.found = true;
            return;
        }
        walk::walk_computed_member_expression(self, it);
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
    if inner.nested_direct_eval {
        return own.names;
    }
    own.names.intersection(&inner.refs).cloned().collect()
}

/// Names referenced from nested functions contained in `stmts`.
///
/// Used for block-scope predeclaration. Function-wide capture analysis is
/// intentionally name-only, but a later closure that captures an outer `x`
/// must not force an unrelated earlier `{ const x }` block binding into a
/// context slot.
#[must_use]
pub fn nested_function_refs_in_statements(stmts: &[Statement<'_>]) -> (HashSet<String>, bool) {
    let mut inner = InnerRefCollector::default();
    for stmt in stmts {
        inner.visit_statement(stmt);
    }
    (inner.refs, inner.nested_direct_eval)
}

#[must_use]
pub fn nested_function_refs_in_statement_refs(stmts: &[&Statement<'_>]) -> (HashSet<String>, bool) {
    let mut inner = InnerRefCollector::default();
    for stmt in stmts {
        inner.visit_statement(stmt);
    }
    (inner.refs, inner.nested_direct_eval)
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
    /// a context slot just like a plain `let` binding.
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
        // closure is not promoted to a context slot and resolves as undefined.
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

    fn visit_catch_parameter(&mut self, it: &oxc_ast::ast::CatchParameter<'a>) {
        self.maybe_collect_pattern(&it.pattern);
        walk::walk_catch_parameter(self, it);
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

/// The names a nested function shadows *everywhere inside itself*.
///
/// Used to decide whether a reference inside a nested function is a capture of
/// an outer binding. Only bindings that cover the whole nested function belong
/// here: its parameters, its `arguments`, the `var`s it declares at any depth
/// (those are function-scoped), and the lexical declarations at the top level
/// of its body.
///
/// A `let` inside a *block* deliberately does not count. It shadows within its
/// block and nowhere else, so counting it would suppress the capture of the
/// outer binding that the rest of the function still refers to — and that
/// binding would then have no context slot, which is a `ReferenceError` at
/// the first reference outside the block.
///
/// The set is deliberately minimal. Naming one name too few costs a context
/// slot that nothing reads; naming one too many costs a reference that
/// resolves to nothing.
fn nested_function_own_names(
    params: Option<&FormalParameters<'_>>,
    body: Option<&FunctionBody<'_>>,
    is_arrow: bool,
) -> HashSet<String> {
    let mut own = OwnNameCollector::default();
    if let Some(p) = params {
        own.visit_formal_parameters(p);
    }
    if !is_arrow {
        own.names.insert("arguments".to_string());
    }
    let Some(body) = body else {
        return own.names;
    };

    // The top level of the body: every declaration here covers the whole
    // function, whatever its kind.
    for statement in &body.statements {
        match statement {
            Statement::VariableDeclaration(declaration) => {
                for declarator in &declaration.declarations {
                    own.collect_pattern_leaves(&declarator.id);
                }
            }
            Statement::FunctionDeclaration(function) => {
                if let Some(id) = function.id.as_ref() {
                    own.names.insert(id.name.as_str().to_string());
                }
            }
            Statement::ClassDeclaration(class) => {
                if let Some(id) = class.id.as_ref() {
                    own.names.insert(id.name.as_str().to_string());
                }
            }
            _ => {}
        }
    }

    // And `var`, wherever it is written: a `var` in a block is a binding of the
    // function, not of the block.
    let mut vars = VarNameCollector::default();
    vars.visit_function_body(body);
    own.names.extend(vars.names);
    own.names
}

/// Every `var` a function declares, at any block depth, excluding those of
/// nested functions.
#[derive(Default)]
struct VarNameCollector {
    names: HashSet<String>,
    nested_depth: u32,
}

impl<'a> Visit<'a> for VarNameCollector {
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

    fn visit_variable_declaration(&mut self, it: &oxc_ast::ast::VariableDeclaration<'a>) {
        if self.nested_depth == 0 && it.kind.is_var() {
            let mut leaves = OwnNameCollector::default();
            for declarator in &it.declarations {
                leaves.collect_pattern_leaves(&declarator.id);
            }
            self.names.extend(leaves.names);
        }
        walk::walk_variable_declaration(self, it);
    }
}

/// Walks a function body and collects the free variables of every nested
/// function (transitively): identifier names referenced from inside a
/// nested function that the nested function (or an intervening one) does
/// not itself bind. A nested function whose own parameter / local shadows
/// an outer name of the same spelling therefore does not mark that outer
/// name as captured.
#[derive(Default)]
struct InnerRefCollector {
    refs: HashSet<String>,
    nested_depth: u32,
    /// Own-name sets of the nested functions currently on the walk stack.
    bound: Vec<HashSet<String>>,
    /// A nested function contains a direct `eval(...)` call: every visible
    /// outer lexical binding is then reachable from that eval, so capture
    /// analysis must promote all of them rather than only the named refs.
    nested_direct_eval: bool,
}

/// Collects every identifier reference in a function body at any
/// depth (direct body refs and nested-closure refs alike). Backs
/// [`body_references_name`]'s "is the self-name observable" question.
#[derive(Default)]
struct AnyRefCollector {
    refs: HashSet<String>,
}

impl<'a> Visit<'a> for AnyRefCollector {
    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        self.refs.insert(it.name.as_str().to_string());
    }
}

impl<'a> Visit<'a> for InnerRefCollector {
    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        // Any direct eval — in this body or in a nested function — can read
        // every visible lexical binding, so the whole own-name set promotes.
        if callee_is_direct_eval(&it.callee)
            && !self.bound.iter().any(|scope| scope.contains("eval"))
        {
            self.nested_direct_eval = true;
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: oxc_syntax::scope::ScopeFlags) {
        let own = nested_function_own_names(Some(&it.params), it.body.as_deref(), false);
        self.bound.push(own);
        self.nested_depth = self.nested_depth.saturating_add(1);
        walk::walk_function(self, it, flags);
        self.nested_depth = self.nested_depth.saturating_sub(1);
        self.bound.pop();
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let own = nested_function_own_names(Some(&it.params), Some(&it.body), true);
        self.bound.push(own);
        self.nested_depth = self.nested_depth.saturating_add(1);
        walk::walk_arrow_function_expression(self, it);
        self.nested_depth = self.nested_depth.saturating_sub(1);
        self.bound.pop();
    }

    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        // A reference is a capture of an *outer* binding only when no nested
        // function on the walk stack binds the same name (a shadowing param
        // or local resolves the reference locally, not to the outer scope).
        if self.nested_depth > 0 {
            let name = it.name.as_str();
            if !self.bound.iter().any(|scope| scope.contains(name)) {
                self.refs.insert(name.to_string());
            }
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
