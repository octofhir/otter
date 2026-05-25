//! Strict-mode early-error validation pass.
//!
//! Walks the parsed program once and rejects ECMA-262 strict-mode
//! early errors that `oxc_parser` does not flag on its own. The pass
//! must run before the bytecode lowering pipeline so it can surface a
//! `SyntaxError` with phase `parse` to the runner.
//!
//! # Contents
//! - [`validate_strict_mode_early_errors`] — public entry called from
//!   `compile_program` / `compile_module_program`.
//!
//! # Invariants
//! - Strictness is tracked as a stack: source-level strict (force,
//!   module mode, or top-level `"use strict"`), function-level strict
//!   (inherited from outer or own-body directive), and class bodies
//!   (unconditionally strict per ECMA-262 §10.2.10).
//! - The walker emits owned [`SyntaxDiagnostic`] entries; no `oxc`
//!   handles cross the crate boundary.
//!
//! # See also
//! - ECMA-262 §12.9.3.1 Static Semantics: Early Errors for
//!   NumericLiteral (LegacyOctalIntegerLiteral and
//!   NonOctalDecimalIntegerLiteral are early errors in strict code):
//!   <https://tc39.es/ecma262/#sec-literals-numeric-literals-static-semantics-early-errors>
//! - ECMA-262 §10.2.10 ClassBody is always strict mode code:
//!   <https://tc39.es/ecma262/#sec-strict-mode-code>

use std::collections::BTreeMap;

use otter_syntax::SyntaxDiagnostic;
use oxc_ast::ast::{
    ArrowFunctionExpression, AssignmentExpression, AssignmentTarget, AwaitExpression,
    BindingIdentifier, BindingPattern, Class, ClassElement, DoWhileStatement, Expression,
    ForInStatement, ForOfStatement, ForStatement, ForStatementLeft, FormalParameters, Function,
    IdentifierReference, IfStatement, LabeledStatement, MethodDefinitionKind, NumericLiteral,
    Program, PropertyKey, SimpleAssignmentTarget, Statement, StaticBlock, StringLiteral,
    SwitchStatement, UnaryExpression, UnaryOperator, UpdateExpression, UpdateOperator,
    VariableDeclarationKind, WhileStatement, YieldExpression,
};
use oxc_ast_visit::{Visit, walk};
use oxc_span::Span;
use oxc_syntax::scope::ScopeFlags;

use crate::CompileError;

/// Validate strict-mode early errors that `oxc_parser` does not raise.
///
/// Returns `Ok(())` when the program is well-formed under strict-mode
/// early-error rules, or [`CompileError::Syntax`] carrying one
/// [`SyntaxDiagnostic`] per violation (preserving order of appearance).
///
/// `force_strict` lets direct-eval callers inherit the caller's
/// strictness without rewriting the source.
pub fn validate_strict_mode_early_errors(
    program: &Program<'_>,
    force_strict: bool,
) -> Result<(), CompileError> {
    // Note: `program.source_type` is unreliable here. `otter-syntax`
    // calls `SourceType::default()` (which is `mjs()` in oxc) for all
    // script and module inputs alike; the script-vs-module routing is
    // performed separately by the host runtime. We therefore derive
    // initial strictness from the caller's `force_strict` (true for
    // module compilation entry and direct-eval inheritance) plus the
    // top-level `"use strict"` directive only.
    let source_strict = force_strict || program.has_use_strict_directive();
    let mut visitor = StrictValidator {
        strict_stack: vec![source_strict],
        diagnostics: Vec::new(),
    };
    visitor.visit_program(program);
    if visitor.diagnostics.is_empty() {
        return Ok(());
    }
    let messages = visitor
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect();
    Err(CompileError::Syntax {
        messages,
        diagnostics: visitor.diagnostics,
    })
}

struct StrictValidator {
    strict_stack: Vec<bool>,
    diagnostics: Vec<SyntaxDiagnostic>,
}

impl StrictValidator {
    fn is_strict(&self) -> bool {
        self.strict_stack.last().copied().unwrap_or(false)
    }

    /// Scan a class field / accessor initializer for free
    /// `arguments` references and emit a diagnostic if any survives
    /// the same scope rules used by [`ContainsArgumentsScanner`].
    fn check_initializer_no_arguments(&mut self, init: &Expression<'_>, label: &str) {
        let mut scanner = ContainsArgumentsScanner { found: None };
        scanner.visit_expression(init);
        if let Some(span) = scanner.found {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "CLASS_FIELD_CONTAINS_ARGUMENTS".to_string(),
                message: format!(
                    "SyntaxError: class {label} cannot reference `arguments` \
                     (§15.7.1 ContainsArguments early error)"
                ),
                range: Some((span.start, span.end)),
                help: Some(
                    "class field initializers run in the class scope, which never binds \
                     `arguments`; pass the values you need explicitly into a method instead"
                        .to_string(),
                ),
            });
        }
    }

    /// Walk a class body and flag duplicate `PrivateBoundNames` per
    /// ECMA-262 §15.7.1.
    ///
    /// Allowed shapes for the same name within one class:
    ///   - exactly one getter + one setter with matching staticness
    ///     (instance accessor pair OR static accessor pair).
    ///
    /// Anything else — two methods, method + field, two fields,
    /// async generator + field, static + instance with the same name,
    /// getter without setter twice, etc. — is a Syntax Error.
    fn check_private_bound_names(&mut self, class: &Class<'_>) {
        // Bucket entries by private name. Each entry records its kind
        // (the spec-relevant role) and staticness so the getter/setter
        // exception can match precisely.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum PrivKind {
            Method,
            Get,
            Set,
            Field,
        }
        let mut buckets: BTreeMap<&str, Vec<(PrivKind, bool, oxc_span::Span)>> = BTreeMap::new();
        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(m) => {
                    if let PropertyKey::PrivateIdentifier(id) = &m.key {
                        let kind = match m.kind {
                            MethodDefinitionKind::Get => PrivKind::Get,
                            MethodDefinitionKind::Set => PrivKind::Set,
                            MethodDefinitionKind::Method => PrivKind::Method,
                            MethodDefinitionKind::Constructor => PrivKind::Method,
                        };
                        buckets
                            .entry(id.name.as_str())
                            .or_default()
                            .push((kind, m.r#static, id.span));
                    }
                }
                ClassElement::PropertyDefinition(p) => {
                    if let PropertyKey::PrivateIdentifier(id) = &p.key {
                        buckets.entry(id.name.as_str()).or_default().push((
                            PrivKind::Field,
                            p.r#static,
                            id.span,
                        ));
                    }
                }
                ClassElement::AccessorProperty(a) => {
                    if let PropertyKey::PrivateIdentifier(id) = &a.key {
                        buckets.entry(id.name.as_str()).or_default().push((
                            PrivKind::Field,
                            a.r#static,
                            id.span,
                        ));
                    }
                }
                _ => {}
            }
        }
        for (name, entries) in &buckets {
            if entries.len() < 2 {
                continue;
            }
            // §15.7.1 exception: exactly one getter and one setter
            // sharing staticness. Anything else is a SyntaxError.
            let allowed = entries.len() == 2 && {
                let (k0, s0, _) = entries[0];
                let (k1, s1, _) = entries[1];
                s0 == s1
                    && ((k0 == PrivKind::Get && k1 == PrivKind::Set)
                        || (k0 == PrivKind::Set && k1 == PrivKind::Get))
            };
            if allowed {
                continue;
            }
            // Flag every entry past the first so each duplicate gets
            // a diagnostic anchored at its own site.
            for (_, _, span) in entries.iter().skip(1) {
                self.diagnostics.push(SyntaxDiagnostic {
                    code: "CLASS_DUPLICATE_PRIVATE_NAME".to_string(),
                    message: format!(
                        "SyntaxError: duplicate private name `{name}` in class body (§15.7.1; \
                         the only allowed dup is one getter + one setter pair with matching \
                         static-ness)"
                    ),
                    range: Some((span.start, span.end)),
                    help: Some(
                        "rename one of the private members, or merge them into a single \
                         getter/setter pair"
                            .to_string(),
                    ),
                });
            }
        }
    }

    /// Flag a function with a `"use strict"` directive in its body
    /// AND a non-simple parameter list. ECMA-262 §15.2.1, §15.3.1,
    /// §15.4.1, §15.6.1 reject the combination because the strict
    /// directive cannot apply to the parameter-initialization step
    /// (which may already have started running expressions through
    /// default values, destructuring binds, etc.).
    fn flag_non_simple_use_strict(&mut self, span: oxc_span::Span) {
        self.diagnostics.push(SyntaxDiagnostic {
            code: "STRICT_DIRECTIVE_WITH_NON_SIMPLE_PARAMS".to_string(),
            message: "SyntaxError: `\"use strict\"` directive cannot appear in a function with a \
                 non-simple parameter list (rest, default, or destructuring) \
                 (§15.2.1 / §15.3.1 / §15.4.1 / §15.6.1)"
                .to_string(),
            range: Some((span.start, span.end)),
            help: Some(
                "remove the `\"use strict\"` directive — the function inherits strictness from \
                 the enclosing scope; or rewrite the parameter list as plain identifiers"
                    .to_string(),
            ),
        });
    }

    /// Flag a `for-in` / `for-of` head whose ForBinding declarator
    /// carries an Initializer.
    ///
    /// - `for-in`: ECMA-262 §13.7.5.1 forbids the Initializer for
    ///   `let` / `const` / `using` / destructuring bindings
    ///   unconditionally, and Annex B §B.3.5 grants `var x = init`
    ///   only in sloppy mode. Strict-mode `var` with an Initializer
    ///   is also a SyntaxError.
    /// - `for-of`: Initializer is never legal regardless of mode.
    ///
    /// `kind_label` is `"for-in"` or `"for-of"`; the helper picks
    /// the right relaxation through the `is_for_of` flag derived
    /// from the label.
    fn flag_for_head_initializer(&mut self, left: &ForStatementLeft<'_>, kind_label: &'static str) {
        let ForStatementLeft::VariableDeclaration(decl) = left else {
            return;
        };
        let is_for_of = kind_label == "for-of";
        let allow_var_init_in_sloppy =
            !self.is_strict() && matches!(decl.kind, VariableDeclarationKind::Var) && !is_for_of;
        for declarator in &decl.declarations {
            let Some(init) = &declarator.init else {
                continue;
            };
            // Destructuring patterns (object / array) never tolerate
            // an Initializer in for-in / for-of heads, even in
            // sloppy mode (Annex B §B.3.5 only relaxes simple `var`).
            let destructuring = !matches!(declarator.id, BindingPattern::BindingIdentifier(_));
            if allow_var_init_in_sloppy && !destructuring {
                continue;
            }
            let init_span = oxc_span::GetSpan::span(init);
            self.diagnostics.push(SyntaxDiagnostic {
                code: "FOR_HEAD_INITIALIZER".to_string(),
                message: format!(
                    "SyntaxError: `{kind_label}` head declarator cannot carry an initializer \
                     (§13.7.5.1; Annex B §B.3.5 grants only sloppy `for (var x = ... in ...)`, \
                     which never extends to `for-of` or destructuring patterns)"
                ),
                range: Some((init_span.start, init_span.end)),
                help: Some(
                    "drop the `= …` initializer from the `for` head; assign the value inside \
                     the loop body instead"
                        .to_string(),
                ),
            });
        }
    }

    /// Flag a `FunctionDeclaration` appearing as the direct body of
    /// a control-flow statement (if / while / for / labeled / etc.).
    ///
    /// ECMA-262 §13.6.1.1 / §13.7.2.1 / §13.7.3.1 / §13.7.4.1 /
    /// §13.7.5.1 / §14.13.1: it is a Syntax Error if
    /// `IsLabelledFunction(Statement)` is true. Annex B §B.3.2
    /// relaxes only the `IfStatement` arm and only for sloppy mode;
    /// strict mode rejects unconditionally for every controller.
    /// `context_label` is folded into the diagnostic so the engine
    /// reports which controller hosted the offender.
    fn flag_function_declaration_body(&mut self, body: &Statement<'_>, context_label: &str) {
        if !self.is_strict() {
            return;
        }
        let Some(span) = function_declaration_body_span(body) else {
            return;
        };
        self.diagnostics.push(SyntaxDiagnostic {
            code: "STRICT_FUNCTION_AS_STATEMENT_BODY".to_string(),
            message: format!(
                "SyntaxError: function declarations cannot appear as the direct body of \
                 `{context_label}` in strict mode (§13.6.1.1 / §13.7.x.1 \
                 IsLabelledFunction early error)"
            ),
            range: Some((span.start, span.end)),
            help: Some(
                "wrap the function declaration in a block `{ … }` or convert it to a function \
                 expression assigned to a binding"
                    .to_string(),
            ),
        });
    }
}

/// Scanner that detects free `arguments` references inside a class
/// static initialization block per ECMA-262 §15.7.1 ContainsArguments.
///
/// "Free" here means *not shadowed by a nested function-like binding
/// scope*. The walker:
/// - Records the first `IdentifierReference` named `arguments`.
/// - Stops descent at `Function`, `ArrowFunctionExpression`, and any
///   nested `StaticBlock` (each opens its own `arguments` scope or
///   sits outside the original block).
/// - Stops descent at class element bodies that have their own
///   `[[ContainsArguments]]` semantics: method definitions
///   (`MethodDefinition.value` is a Function), property / accessor
///   initializers (treated as function-like FieldDefinition records),
///   and inner `StaticBlock`s — but still walks computed property
///   keys, which evaluate in the surrounding scope and therefore see
///   the outer `arguments` ban.
/// - Walks `Class.super_class` because the heritage expression is
///   evaluated in the surrounding scope.
struct ContainsArgumentsScanner {
    found: Option<oxc_span::Span>,
}

impl<'a> Visit<'a> for ContainsArgumentsScanner {
    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        if self.found.is_none() && it.name.as_str() == "arguments" {
            self.found = Some(it.span);
        }
    }

    // Bail at function / arrow boundaries (own arguments scope).
    fn visit_function(&mut self, _: &Function<'a>, _: ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _: &ArrowFunctionExpression<'a>) {}
    // A nested class static block is its own ContainsArguments scope.
    fn visit_static_block(&mut self, _: &StaticBlock<'a>) {}

    // Walk class shapes selectively: heritage expression + computed
    // keys live in surrounding scope; method values, field
    // initializers, and inner static blocks each open their own
    // boundary and are skipped by the per-shape visitors above.
    fn visit_class(&mut self, it: &Class<'a>) {
        if let Some(super_class) = &it.super_class {
            self.visit_expression(super_class);
        }
        for element in &it.body.body {
            match element {
                ClassElement::MethodDefinition(m) => {
                    if m.computed {
                        self.visit_property_key(&m.key);
                    }
                }
                ClassElement::PropertyDefinition(p) => {
                    if p.computed {
                        self.visit_property_key(&p.key);
                    }
                    // p.value is a field initializer with its own scope — skip.
                }
                ClassElement::AccessorProperty(a) => {
                    if a.computed {
                        self.visit_property_key(&a.key);
                    }
                }
                ClassElement::StaticBlock(_) | ClassElement::TSIndexSignature(_) => {}
            }
        }
    }
}

/// ECMA-262 §15.1.3 IsSimpleParameterList.
///
/// A FormalParameterList is "simple" iff every entry is a bare
/// `BindingIdentifier` with no initializer and no rest parameter is
/// present. Anything else — `{a}`, `[b]`, `x = 1`, `...rest`, an
/// AssignmentPattern wrapper — produces `false`.
///
/// The result drives §15.2.1 / §15.3.1 / §15.4.1 / §15.6.1 early
/// errors: a function with a `"use strict"` directive in its body
/// must have a simple parameter list, otherwise the directive
/// cannot consistently apply to default-value / destructuring
/// initialization that runs before the body.
fn is_simple_parameter_list(params: &FormalParameters<'_>) -> bool {
    if params.rest.is_some() {
        return false;
    }
    params.items.iter().all(|p| {
        p.initializer.is_none() && matches!(p.pattern, BindingPattern::BindingIdentifier(_))
    })
}

fn collect_param_bound_names<'a, F>(params: &'a FormalParameters<'a>, emit: &mut F)
where
    F: FnMut(&'a str, oxc_span::Span),
{
    for param in &params.items {
        for_each_bound_identifier(&param.pattern, emit);
    }
    if let Some(rest) = &params.rest {
        for_each_bound_identifier(&rest.rest.argument, emit);
    }
}

/// Collect names introduced by **lexical** declarations directly
/// inside `stmts` — `let`, `const`, `class`, and every
/// `FunctionDeclaration` shape (sync, async, generator, async
/// generator). These names participate in §13.12.1
/// `LexicallyDeclaredNames` of a switch CaseBlock.
///
/// The walk is **non-recursive** with respect to nested blocks: a
/// `BlockStatement` introduces its own scope, so its inner lexical
/// bindings do not bubble up to the switch CaseBlock. The walk does
/// recurse through `LabeledStatement` because a labelled lexical
/// declaration still contributes to the surrounding CaseBlock.
fn collect_lex_decl_names<'a, F>(stmts: &'a [Statement<'a>], emit: &mut F)
where
    F: FnMut(&'a str, oxc_span::Span),
{
    for stmt in stmts {
        match stmt {
            Statement::VariableDeclaration(decl)
                if matches!(
                    decl.kind,
                    VariableDeclarationKind::Let
                        | VariableDeclarationKind::Const
                        | VariableDeclarationKind::Using
                        | VariableDeclarationKind::AwaitUsing
                ) =>
            {
                for declarator in &decl.declarations {
                    for_each_bound_identifier(&declarator.id, &mut |name, span| {
                        emit(name, span);
                    });
                }
            }
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    emit(id.name.as_str(), id.span);
                }
            }
            Statement::ClassDeclaration(cls) => {
                if let Some(id) = &cls.id {
                    emit(id.name.as_str(), id.span);
                }
            }
            Statement::LabeledStatement(labelled) => {
                if let Some(inner) = std::slice::from_ref(&labelled.body).first() {
                    collect_lex_decl_names(std::slice::from_ref(inner), emit);
                }
            }
            _ => {}
        }
    }
}

/// Collect names introduced by `var` declarations anywhere in
/// `stmts` (recursing through control-flow constructs and nested
/// blocks but stopping at function / class boundaries). These names
/// form `VarDeclaredNames` of the surrounding CaseBlock per
/// §13.12.1, and they must not clash with `LexicallyDeclaredNames`
/// inside the same switch.
fn collect_var_decl_names<'a, F>(stmts: &'a [Statement<'a>], emit: &mut F)
where
    F: FnMut(&'a str, oxc_span::Span),
{
    for stmt in stmts {
        collect_var_decl_names_in_stmt(stmt, emit);
    }
}

fn collect_var_decl_names_in_stmt<'a, F>(stmt: &'a Statement<'a>, emit: &mut F)
where
    F: FnMut(&'a str, oxc_span::Span),
{
    match stmt {
        Statement::VariableDeclaration(decl)
            if matches!(decl.kind, VariableDeclarationKind::Var) =>
        {
            for declarator in &decl.declarations {
                for_each_bound_identifier(&declarator.id, &mut |name, span| {
                    emit(name, span);
                });
            }
        }
        Statement::BlockStatement(block) => collect_var_decl_names(&block.body, emit),
        Statement::IfStatement(ifs) => {
            collect_var_decl_names_in_stmt(&ifs.consequent, emit);
            if let Some(alt) = &ifs.alternate {
                collect_var_decl_names_in_stmt(alt, emit);
            }
        }
        Statement::DoWhileStatement(s) => collect_var_decl_names_in_stmt(&s.body, emit),
        Statement::WhileStatement(s) => collect_var_decl_names_in_stmt(&s.body, emit),
        Statement::ForStatement(s) => {
            if let Some(init) = &s.init
                && let oxc_ast::ast::ForStatementInit::VariableDeclaration(decl) = init
                && matches!(decl.kind, VariableDeclarationKind::Var)
            {
                for declarator in &decl.declarations {
                    for_each_bound_identifier(&declarator.id, &mut |name, span| {
                        emit(name, span);
                    });
                }
            }
            collect_var_decl_names_in_stmt(&s.body, emit);
        }
        Statement::ForInStatement(s) => collect_var_decl_names_in_stmt(&s.body, emit),
        Statement::ForOfStatement(s) => collect_var_decl_names_in_stmt(&s.body, emit),
        Statement::SwitchStatement(s) => {
            for case in &s.cases {
                collect_var_decl_names(&case.consequent, emit);
            }
        }
        Statement::LabeledStatement(s) => collect_var_decl_names_in_stmt(&s.body, emit),
        Statement::TryStatement(s) => {
            collect_var_decl_names(&s.block.body, emit);
            if let Some(handler) = &s.handler {
                collect_var_decl_names(&handler.body.body, emit);
            }
            if let Some(finalizer) = &s.finalizer {
                collect_var_decl_names(&finalizer.body, emit);
            }
        }
        _ => {}
    }
}

/// Walk a `BindingPattern` and emit every bound identifier name with
/// its span. Used to enumerate the `BoundNames` of a `VariableDeclarator`
/// (§8.2.1) for both lexical and var collection helpers above.
fn for_each_bound_identifier<'a, F>(pattern: &'a BindingPattern<'a>, emit: &mut F)
where
    F: FnMut(&'a str, oxc_span::Span),
{
    match pattern {
        BindingPattern::BindingIdentifier(id) => emit(id.name.as_str(), id.span),
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                for_each_bound_identifier(&prop.value, emit);
            }
            if let Some(rest) = &obj.rest {
                for_each_bound_identifier(&rest.argument, emit);
            }
        }
        BindingPattern::ArrayPattern(arr) => {
            for p in arr.elements.iter().flatten() {
                for_each_bound_identifier(p, emit);
            }
            if let Some(rest) = &arr.rest {
                for_each_bound_identifier(&rest.argument, emit);
            }
        }
        BindingPattern::AssignmentPattern(asn) => for_each_bound_identifier(&asn.left, emit),
    }
}

struct ContainsYieldScanner {
    found: Option<Span>,
}

impl<'a> Visit<'a> for ContainsYieldScanner {
    fn visit_yield_expression(&mut self, it: &YieldExpression<'a>) {
        if self.found.is_none() {
            self.found = Some(it.span);
        }
    }

    fn visit_function(&mut self, _: &Function<'a>, _: ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _: &ArrowFunctionExpression<'a>) {}
    fn visit_static_block(&mut self, _: &StaticBlock<'a>) {}
}

struct ContainsAwaitScanner {
    found: Option<Span>,
}

impl<'a> Visit<'a> for ContainsAwaitScanner {
    fn visit_await_expression(&mut self, it: &AwaitExpression<'a>) {
        if self.found.is_none() {
            self.found = Some(it.span);
        }
    }

    fn visit_function(&mut self, _: &Function<'a>, _: ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _: &ArrowFunctionExpression<'a>) {}
    fn visit_static_block(&mut self, _: &StaticBlock<'a>) {}
}

/// Return the source span of a `FunctionDeclaration` when `body` is
/// that exact AST shape (rather than e.g. a `BlockStatement` or a
/// `LabeledStatement` that wraps one). Used by the strict-mode body
/// check so the diagnostic points at the offending function header.
fn function_declaration_body_span(body: &Statement<'_>) -> Option<Span> {
    match body {
        Statement::FunctionDeclaration(func) => Some(func.span),
        _ => None,
    }
}

impl<'a> Visit<'a> for StrictValidator {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let body_strict = it
            .body
            .as_ref()
            .is_some_and(|b| b.has_use_strict_directive());
        // §15.2.1 FunctionDeclaration / §15.3.1 FunctionExpression
        // Static Semantics: Early Errors — a `"use strict"` directive
        // is forbidden inside a function whose FormalParameterList is
        // non-simple (rest parameter, default initializer, or
        // destructuring pattern). The directive itself would be a
        // SyntaxError so we flag at the function span.
        if body_strict && !is_simple_parameter_list(&it.params) {
            self.flag_non_simple_use_strict(it.span);
        }
        let inner_strict = self.is_strict() || body_strict;
        self.strict_stack.push(inner_strict);
        walk::walk_function(self, it, flags);
        self.strict_stack.pop();
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let body_strict = it.body.has_use_strict_directive();
        // §15.4.1 ArrowFunction / §15.6.1 AsyncArrowFunction Static
        // Semantics: Early Errors — same `"use strict"` /
        // non-simple-params restriction applies.
        if body_strict && !is_simple_parameter_list(&it.params) {
            self.flag_non_simple_use_strict(it.span);
        }
        let mut yield_scanner = ContainsYieldScanner { found: None };
        yield_scanner.visit_formal_parameters(&it.params);
        if let Some(span) = yield_scanner.found {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "ARROW_PARAMS_CONTAIN_YIELD".to_string(),
                message:
                    "SyntaxError: ArrowParameters must not contain a YieldExpression (§15.4.1)"
                        .to_string(),
                range: Some((span.start, span.end)),
                help: Some("move `yield` out of the arrow parameter list".to_string()),
            });
        }
        let mut await_scanner = ContainsAwaitScanner { found: None };
        await_scanner.visit_formal_parameters(&it.params);
        if let Some(span) = await_scanner.found {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "ARROW_PARAMS_CONTAIN_AWAIT".to_string(),
                message:
                    "SyntaxError: ArrowParameters must not contain an AwaitExpression (§15.4.1)"
                        .to_string(),
                range: Some((span.start, span.end)),
                help: Some("move `await` out of the arrow parameter list".to_string()),
            });
        }
        let mut param_names: BTreeMap<&str, Span> = BTreeMap::new();
        collect_param_bound_names(&it.params, &mut |name, span| {
            param_names.entry(name).or_insert(span);
        });
        collect_lex_decl_names(&it.body.statements, &mut |name, span| {
            if param_names.contains_key(name) {
                self.diagnostics.push(SyntaxDiagnostic {
                    code: "ARROW_PARAM_LEXICAL_REDECLARATION".to_string(),
                    message: format!(
                        "SyntaxError: arrow parameter `{name}` conflicts with a lexical declaration in the body (§15.4.1)"
                    ),
                    range: Some((span.start, span.end)),
                    help: Some(
                        "rename either the parameter or the lexical declaration".to_string(),
                    ),
                });
            }
        });
        let inner_strict = self.is_strict() || body_strict;
        self.strict_stack.push(inner_strict);
        walk::walk_arrow_function_expression(self, it);
        self.strict_stack.pop();
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        // ECMA-262 §10.2.10 — class bodies are always strict mode code.
        self.strict_stack.push(true);
        // §15.7.1 ClassBody Static Semantics: Early Errors —
        // PrivateBoundNames of ClassBody contains no duplicate
        // entries, except when a name is used exactly once as a
        // getter and once as a setter (no other entries).
        self.check_private_bound_names(it);
        // §15.7.1 FieldDefinition Static Semantics: Early Errors —
        // It is a Syntax Error if Initializer is present and
        // ContainsArguments of Initializer is true. Field initializers
        // (and accessor `value` expressions) execute in the class
        // scope, which never binds `arguments` even when the class
        // sits inside a function.
        for element in &it.body.body {
            match element {
                ClassElement::PropertyDefinition(p) => {
                    if let Some(init) = &p.value {
                        self.check_initializer_no_arguments(init, "field initializer");
                    }
                }
                ClassElement::AccessorProperty(a) => {
                    if let Some(init) = &a.value {
                        self.check_initializer_no_arguments(init, "accessor initializer");
                    }
                }
                _ => {}
            }
        }
        walk::walk_class(self, it);
        self.strict_stack.pop();
    }

    fn visit_static_block(&mut self, it: &StaticBlock<'a>) {
        // §15.7.1 ClassStaticBlockBody Static Semantics: Early
        // Errors — `arguments` is not in scope inside a class static
        // initialization block, so any IdentifierReference resolving
        // to the name `arguments` is a SyntaxError. We use a custom
        // scanner that walks the block statements but stops at any
        // nested scope that binds its own `arguments` (functions,
        // methods, class static blocks, property / accessor
        // initializers).
        let mut scanner = ContainsArgumentsScanner { found: None };
        for stmt in &it.body {
            scanner.visit_statement(stmt);
            if scanner.found.is_some() {
                break;
            }
        }
        if let Some(span) = scanner.found {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "CLASS_STATIC_BLOCK_CONTAINS_ARGUMENTS".to_string(),
                message:
                    "SyntaxError: class static initialization block cannot reference `arguments` \
                     (§15.7.1 ContainsArguments early error)"
                        .to_string(),
                range: Some((span.start, span.end)),
                help: Some(
                    "use a class field initializer or move the logic into a method; \
                     `arguments` has no binding inside a static block"
                        .to_string(),
                ),
            });
        }
        walk::walk_static_block(self, it);
    }

    fn visit_numeric_literal(&mut self, it: &NumericLiteral<'a>) {
        if !self.is_strict() {
            return;
        }
        let Some(raw) = it.raw else {
            return;
        };
        if is_legacy_numeric_form(raw.as_str()) {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "STRICT_LEGACY_NUMERIC".to_string(),
                message: format!(
                    "SyntaxError: legacy octal or non-octal-decimal integer literal `{}` is not allowed in strict mode",
                    raw.as_str()
                ),
                range: Some((it.span.start, it.span.end)),
                help: Some(
                    "use the `0o` prefix for octal literals in strict mode code".to_string(),
                ),
            });
        }
    }

    fn visit_switch_statement(&mut self, it: &SwitchStatement<'a>) {
        // ECMA-262 §13.12.1 SwitchStatement Static Semantics: Early
        // Errors:
        //   - It is a Syntax Error if the LexicallyDeclaredNames of
        //     CaseBlock contains any duplicate entries.
        //   - It is a Syntax Error if any element of the
        //     LexicallyDeclaredNames of CaseBlock also occurs in the
        //     VarDeclaredNames of CaseBlock.
        //
        // Independent of strict mode — these are static errors for
        // every switch.
        let mut lex_seen: BTreeMap<&str, ()> = BTreeMap::new();
        let mut var_seen: BTreeMap<&str, ()> = BTreeMap::new();
        let mut duplicates: Vec<(String, oxc_span::Span)> = Vec::new();
        let mut lex_var_clash: Vec<(String, oxc_span::Span)> = Vec::new();

        for case in &it.cases {
            collect_lex_decl_names(&case.consequent, &mut |name, span| {
                if lex_seen.insert(name, ()).is_some() {
                    duplicates.push((name.to_string(), span));
                }
            });
            collect_var_decl_names(&case.consequent, &mut |name, _span| {
                var_seen.insert(name, ());
            });
        }
        for case in &it.cases {
            collect_lex_decl_names(&case.consequent, &mut |name, span| {
                if var_seen.contains_key(name) {
                    lex_var_clash.push((name.to_string(), span));
                }
            });
        }

        for (name, span) in &duplicates {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "SWITCH_DUPLICATE_LEXICAL_DECL".to_string(),
                message: format!(
                    "SyntaxError: `{name}` already lexically declared earlier in this switch \
                     (§13.12.1 LexicallyDeclaredNames of CaseBlock must be unique)"
                ),
                range: Some((span.start, span.end)),
                help: Some(
                    "let / const / class / function declarations across all `case` and \
                     `default` clauses share one lexical scope — rename one binding"
                        .to_string(),
                ),
            });
        }
        for (name, span) in &lex_var_clash {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "SWITCH_LEXICAL_VAR_CLASH".to_string(),
                message: format!(
                    "SyntaxError: lexical declaration of `{name}` conflicts with a `var` \
                     declaration in the same switch CaseBlock (§13.12.1)"
                ),
                range: Some((span.start, span.end)),
                help: Some(
                    "lexical bindings (let / const / class / function) cannot share a name \
                     with a `var` declaration in the surrounding switch"
                        .to_string(),
                ),
            });
        }
        walk::walk_switch_statement(self, it);
    }

    fn visit_if_statement(&mut self, it: &IfStatement<'a>) {
        self.flag_function_declaration_body(&it.consequent, "if");
        if let Some(alt) = &it.alternate {
            self.flag_function_declaration_body(alt, "else");
        }
        walk::walk_if_statement(self, it);
    }

    fn visit_while_statement(&mut self, it: &WhileStatement<'a>) {
        self.flag_function_declaration_body(&it.body, "while");
        walk::walk_while_statement(self, it);
    }

    fn visit_do_while_statement(&mut self, it: &DoWhileStatement<'a>) {
        self.flag_function_declaration_body(&it.body, "do-while");
        walk::walk_do_while_statement(self, it);
    }

    fn visit_for_statement(&mut self, it: &ForStatement<'a>) {
        self.flag_function_declaration_body(&it.body, "for");
        walk::walk_for_statement(self, it);
    }

    fn visit_for_in_statement(&mut self, it: &ForInStatement<'a>) {
        self.flag_function_declaration_body(&it.body, "for-in");
        // §13.7.5.1 ForIn/OfHeadEvaluation early error: ForBinding
        // declarators in a `for-in` head must not carry an
        // Initializer. Annex B §B.3.5 relaxes this for `for (var x =
        // init in obj)` in sloppy mode only; let / const / using and
        // destructuring patterns are always rejected, and `var` with
        // an Initializer is rejected in strict code.
        self.flag_for_head_initializer(&it.left, "for-in");
        walk::walk_for_in_statement(self, it);
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        self.flag_function_declaration_body(&it.body, "for-of");
        // §13.7.5.1 — `for-of` heads never permit Initializer on
        // any variant of ForBinding, regardless of strict mode.
        self.flag_for_head_initializer(&it.left, "for-of");
        walk::walk_for_of_statement(self, it);
    }

    fn visit_labeled_statement(&mut self, it: &LabeledStatement<'a>) {
        // §14.13.1 LabelledStatement Static Semantics: Early Errors —
        // IsLabelledFunction(Statement) must not be true. Even in
        // sloppy mode a labelled function declaration as the labelled
        // body is forbidden; in strict mode all FunctionDeclaration
        // bodies are too, so we flag uniformly.
        self.flag_function_declaration_body(&it.body, "labeled");
        walk::walk_labeled_statement(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        // ECMA-262 §13.1 Identifiers Static Semantics: Early Errors —
        // in strict-mode code an `IdentifierReference` must not be one
        // of the strict-mode FutureReservedWords or `yield`. The
        // parser already rejects `yield` inside generator bodies as a
        // YieldExpression rather than an `IdentifierReference`, so any
        // surviving `IdentifierReference` named `yield` reaches this
        // pass and is a syntax error in strict code (e.g.
        // `[ x = yield ] = []` in a strict script).
        // <https://tc39.es/ecma262/#sec-identifiers-static-semantics-early-errors>
        if self.is_strict() {
            let name = it.name.as_str();
            if matches!(
                name,
                "yield"
                    | "implements"
                    | "interface"
                    | "let"
                    | "package"
                    | "private"
                    | "protected"
                    | "public"
                    | "static"
            ) {
                self.diagnostics.push(SyntaxDiagnostic {
                    code: "STRICT_RESERVED_IDENTIFIER_REFERENCE".to_string(),
                    message: format!(
                        "SyntaxError: `{name}` is a reserved word in strict mode and \
                         cannot appear as an IdentifierReference (§13.1)"
                    ),
                    range: Some((it.span.start, it.span.end)),
                    help: Some(
                        "rename the reference; strict-mode reserved words may not be \
                         used as identifier references"
                            .to_string(),
                    ),
                });
            }
        }
    }

    fn visit_binding_identifier(&mut self, it: &BindingIdentifier<'a>) {
        // ECMA-262 §13.1.1 Static Semantics: Early Errors for
        // BindingIdentifier — in strict mode code, the binding name
        // must not be `eval` or `arguments` (§10.2.1, the strict-mode
        // restriction repeated across §14.1.2, §14.7.4, §15.7.1,
        // §15.10.1, etc.). One hook covers `var eval`, `let
        // arguments`, `function eval() {}`, `class eval {}`, formal
        // parameter `function f(eval) {}`, catch parameter `try {}
        // catch (arguments) {}`, destructuring target names, etc.
        if !self.is_strict() {
            return;
        }
        let name = it.name.as_str();
        if is_reserved_strict_binding_name(name) {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "STRICT_RESERVED_BINDING".to_string(),
                message: format!(
                    "SyntaxError: cannot bind name `{name}` in strict mode code (§13.1.1 \
                     BindingIdentifier reserves `eval`, `arguments`, and the strict-mode \
                     FutureReservedWords `implements` / `interface` / `let` / `package` / \
                     `private` / `protected` / `public` / `static` / `yield`)"
                ),
                range: Some((it.span.start, it.span.end)),
                help: Some(
                    "rename the binding; these identifiers are reserved by the strict-mode \
                     grammar and cannot be declared as bindings"
                        .to_string(),
                ),
            });
        }
    }

    fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
        if self.is_strict()
            && let Some(name) = assignment_target_identifier(&it.left)
            && is_reserved_strict_assignment_target(name)
        {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "STRICT_RESERVED_ASSIGNMENT_TARGET".to_string(),
                message: format!(
                    "SyntaxError: `{name}` is not a valid assignment target in strict mode \
                     (§12.7.1 IsValidSimpleAssignmentTarget reserves `eval` and `arguments`)"
                ),
                range: Some((it.span.start, it.span.end)),
                help: Some(
                    "rename the binding; `eval` and `arguments` cannot be reassigned in strict code"
                        .to_string(),
                ),
            });
        }
        walk::walk_assignment_expression(self, it);
    }

    fn visit_update_expression(&mut self, it: &UpdateExpression<'a>) {
        // §13.4 (Update Expressions): ++/-- on `eval` or `arguments`
        // in strict mode is also an early error via the same
        // simple-assignment-target rule.
        if self.is_strict()
            && matches!(
                it.operator,
                UpdateOperator::Increment | UpdateOperator::Decrement
            )
            && let Some(name) = update_target_identifier(&it.argument)
            && is_reserved_strict_assignment_target(name)
        {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "STRICT_RESERVED_UPDATE_TARGET".to_string(),
                message: format!(
                    "SyntaxError: `{name}` is not a valid update target in strict mode"
                ),
                range: Some((it.span.start, it.span.end)),
                help: Some(
                    "rename the binding; `eval` and `arguments` cannot be incremented or \
                     decremented in strict code"
                        .to_string(),
                ),
            });
        }
        walk::walk_update_expression(self, it);
    }

    fn visit_unary_expression(&mut self, it: &UnaryExpression<'a>) {
        if matches!(it.operator, UnaryOperator::Delete) {
            // §13.5.1.1: `delete <IdentifierReference>` in strict.
            if self.is_strict()
                && let Some(name) = unwrap_parens_identifier(&it.argument)
            {
                self.diagnostics.push(SyntaxDiagnostic {
                    code: "STRICT_DELETE_IDENTIFIER".to_string(),
                    message: format!(
                        "SyntaxError: `delete {name}` is not allowed in strict mode \
                         (UnaryExpression :: delete UnaryExpression resolves to an IdentifierReference)"
                    ),
                    range: Some((it.span.start, it.span.end)),
                    help: Some(
                        "delete a property of an object instead (`delete obj.prop` or \
                         `delete obj[key]`)"
                            .to_string(),
                    ),
                });
            }
            // §13.5.1.1: `delete MemberExpression.PrivateName` and
            // `delete CallExpression.PrivateName` are always Syntax
            // Errors (mode-independent — class bodies are strict
            // anyway, but the rule also rejects the construct in any
            // surrounding scope reachable to a `#name`).
            if let Some(span) = unwrap_parens_private_field_delete_span(&it.argument) {
                self.diagnostics.push(SyntaxDiagnostic {
                    code: "DELETE_PRIVATE_FIELD".to_string(),
                    message: "SyntaxError: cannot `delete` a member of an object's private field \
                         (`delete obj.#name` / `delete expr().#name` are early errors per \
                         §13.5.1.1)"
                        .to_string(),
                    range: Some((span.start, span.end)),
                    help: Some(
                        "private fields cannot be removed; use `obj.#name = undefined` to \
                         clear the slot value if needed"
                            .to_string(),
                    ),
                });
            }
        }
        walk::walk_unary_expression(self, it);
    }

    fn visit_string_literal(&mut self, it: &StringLiteral<'a>) {
        if !self.is_strict() {
            return;
        }
        let Some(raw) = it.raw else {
            return;
        };
        if let Some((rel_start, rel_end)) = find_legacy_string_escape(raw.as_str()) {
            let abs_start = it.span.start + rel_start as u32;
            let abs_end = it.span.start + rel_end as u32;
            self.diagnostics.push(SyntaxDiagnostic {
                code: "STRICT_LEGACY_ESCAPE".to_string(),
                message:
                    "SyntaxError: legacy octal or non-octal-decimal escape sequence is not allowed in strict mode string literal"
                        .to_string(),
                range: Some((abs_start, abs_end)),
                help: Some(
                    "use the `\\xNN` or `\\uNNNN` escape forms in strict mode code".to_string(),
                ),
            });
        }
    }
}

/// Return the bare identifier name when the assignment target is a
/// simple identifier — peeling through ParenthesizedExpression isn't
/// needed at the AssignmentTarget layer because oxc represents
/// `(eval) = 1` differently from a UnaryExpression argument.
fn assignment_target_identifier<'a>(target: &'a AssignmentTarget<'a>) -> Option<&'a str> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(id) => Some(id.name.as_str()),
        _ => None,
    }
}

/// Strict-mode IdentifierReference targets recognised by
/// §12.7.1 IsValidSimpleAssignmentTarget. The bindings `eval` and
/// `arguments` cannot be reassigned, updated, or used as the LHS
/// of a destructuring pattern in strict code.
const STRICT_RESERVED_TARGETS: &[&str] = &["eval", "arguments"];

/// Strict-mode FutureReservedWords flagged by §13.1.1 Static
/// Semantics: Early Errors for BindingIdentifier. The StringValue of
/// the IdentifierName cannot be any of these in strict-mode code,
/// regardless of whether the source uses a Unicode escape sequence
/// — oxc stores the cooked name, so `class let {}` lands here
/// with name="let".
///
/// `yield` is included because the §13.1.1 rule applies in strict
/// scopes even outside generators (the contextual keyword status
/// promotes to a hard reservation under strict-mode semantics).
/// `eval` and `arguments` are repeated so a single helper covers
/// both the §12.7.1 simple-assignment-target rule and the §13.1.1
/// binding rule.
const STRICT_RESERVED_BINDING_NAMES: &[&str] = &[
    "eval",
    "arguments",
    "implements",
    "interface",
    "let",
    "package",
    "private",
    "protected",
    "public",
    "static",
    "yield",
];

#[inline]
fn is_reserved_strict_assignment_target(name: &str) -> bool {
    STRICT_RESERVED_TARGETS.contains(&name)
}

#[inline]
fn is_reserved_strict_binding_name(name: &str) -> bool {
    STRICT_RESERVED_BINDING_NAMES.contains(&name)
}

/// Return the bare identifier name when the update operand is a
/// simple IdentifierReference (`++x` / `x--`).
///
/// `SimpleAssignmentTarget` represents `MemberExpression` variants
/// through the `inherit_variants!` macro; we only care about the
/// `AssignmentTargetIdentifier` arm here because the strict-mode
/// reserved-name rule only applies to bare identifier targets.
fn update_target_identifier<'a>(target: &'a SimpleAssignmentTarget<'a>) -> Option<&'a str> {
    match target {
        SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => Some(id.name.as_str()),
        _ => None,
    }
}

/// Unwrap any number of `ParenthesizedExpression` layers and return
/// the bare identifier name if the resulting expression is an
/// IdentifierReference.
///
/// ECMA-262 §13.5.1.1 Static Semantics: Early Errors flags
/// `delete UnaryExpression` whenever the UnaryExpression is a
/// PrimaryExpression :: IdentifierReference, regardless of how many
/// `(` `)` cover groups wrap it. The check must therefore peel
/// parens before matching.
fn unwrap_parens_identifier<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    let mut cursor = expr;
    loop {
        match cursor {
            Expression::Identifier(id) => return Some(id.name.as_str()),
            Expression::ParenthesizedExpression(inner) => {
                cursor = &inner.expression;
            }
            _ => return None,
        }
    }
}

/// Detect `delete <expr>.#<priv>` (parens-tolerant). Returns the
/// span of the offending `PrivateFieldExpression` so the diagnostic
/// can point at the private name.
///
/// ECMA-262 §13.5.1.1 Static Semantics: Early Errors:
/// > It is a Syntax Error if the derived UnaryExpression is
/// > MemberExpression :: MemberExpression `.` PrivateIdentifier or
/// > CallExpression :: CallExpression `.` PrivateIdentifier.
///
/// Both AST shapes serialize to `PrivateFieldExpression` in oxc, so
/// one branch covers both. The rule is mode-independent — class
/// bodies are strict by §10.2.10 anyway, but the early error also
/// applies wherever a private name is reachable.
fn unwrap_parens_private_field_delete_span<'a>(expr: &'a Expression<'a>) -> Option<oxc_span::Span> {
    let mut cursor = expr;
    loop {
        match cursor {
            Expression::PrivateFieldExpression(pf) => return Some(pf.span),
            Expression::ParenthesizedExpression(inner) => cursor = &inner.expression,
            _ => return None,
        }
    }
}

/// Locate the first `LegacyOctalEscapeSequence` or
/// `NonOctalDecimalEscapeSequence` inside a raw string-literal source
/// fragment (including the enclosing quotes).
///
/// Returns the relative byte range of the offending escape so the
/// caller can map it back to absolute source positions via
/// [`oxc_span::Span::start`].
///
/// # Algorithm (ECMA-262 §12.9.4.1 Static Semantics: Early Errors)
/// Walk the raw bytes with a backslash flag. On encountering an
/// unescaped `\`:
/// - `\` followed by `1..=9` is always rejected
///   (LegacyOctalEscapeSequence for `1..=7`, NonOctalDecimalEscapeSequence
///   for `8..=9`).
/// - `\0` followed by an ASCII digit is rejected
///   (`\05`, `\012`, ... — LegacyOctalEscapeSequence variant
///   starting with the `0` octet).
/// - `\0` followed by anything else (or end of string) is the legal
///   `<NUL>` escape and is skipped.
/// - `\\` consumes both bytes (escaped backslash, not a new escape
///   start).
/// - All other escapes (`\n`, `\t`, `\x..`, `\u..`, `\'`, `\"`, ...)
///   skip the two-byte escape pair.
///
/// Byte-level scanning is safe because every relevant prefix is
/// pure ASCII; multi-byte UTF-8 sequences cannot start with `\` or
/// an ASCII digit.
fn find_legacy_string_escape(raw: &str) -> Option<(usize, usize)> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            i += 1;
            continue;
        }
        let next = bytes[i + 1];
        match next {
            b'\\' => i += 2,
            b'1'..=b'9' => return Some((i, i + 2)),
            b'0' => {
                if let Some(&after) = bytes.get(i + 2)
                    && after.is_ascii_digit()
                {
                    return Some((i, i + 3));
                }
                i += 2;
            }
            _ => i += 2,
        }
    }
    None
}

/// Detect `LegacyOctalIntegerLiteral` and `NonOctalDecimalIntegerLiteral`
/// raw source forms.
///
/// Both productions begin with `0` followed immediately by an ASCII
/// digit. Modern integer prefixes (`0x`, `0o`, `0b`), the `0n`
/// BigInt suffix, fractional / exponent forms (`0.5`, `0e1`), and
/// the bare `0` literal are excluded by checking that the second
/// character is in `0..=9`.
fn is_legacy_numeric_form(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'0' {
        return false;
    }
    bytes[1].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_legacy_octal_forms() {
        assert!(is_legacy_numeric_form("00"));
        assert!(is_legacy_numeric_form("010"));
        assert!(is_legacy_numeric_form("0123"));
        // NonOctalDecimalIntegerLiteral
        assert!(is_legacy_numeric_form("08"));
        assert!(is_legacy_numeric_form("089"));
    }

    #[test]
    fn ignores_modern_numeric_forms() {
        assert!(!is_legacy_numeric_form("0"));
        assert!(!is_legacy_numeric_form("0x1F"));
        assert!(!is_legacy_numeric_form("0o17"));
        assert!(!is_legacy_numeric_form("0b101"));
        assert!(!is_legacy_numeric_form("0n"));
        assert!(!is_legacy_numeric_form("0.5"));
        assert!(!is_legacy_numeric_form("0e1"));
        assert!(!is_legacy_numeric_form("123"));
        assert!(!is_legacy_numeric_form(""));
    }

    #[test]
    fn detects_legacy_string_escapes() {
        // \1..\7 — LegacyOctalEscapeSequence
        assert!(find_legacy_string_escape("\"\\1\"").is_some());
        assert!(find_legacy_string_escape("'\\7'").is_some());
        // \05 — LegacyOctalEscapeSequence starting with 0
        assert!(find_legacy_string_escape("\"\\05\"").is_some());
        assert!(find_legacy_string_escape("\"\\012\"").is_some());
        // \8, \9 — NonOctalDecimalEscapeSequence
        assert!(find_legacy_string_escape("\"\\8\"").is_some());
        assert!(find_legacy_string_escape("\"\\9\"").is_some());
        // Mid-string occurrence
        assert!(find_legacy_string_escape("\"abc\\1def\"").is_some());
    }

    #[test]
    fn ignores_modern_string_escapes() {
        // Bare NUL — allowed when followed by non-digit / end.
        assert!(find_legacy_string_escape("\"\\0\"").is_none());
        // Standard escapes.
        for s in [
            "\"\\n\"", "\"\\t\"", "\"\\r\"", "\"\\b\"", "\"\\f\"", "\"\\v\"",
        ] {
            assert!(find_legacy_string_escape(s).is_none(), "rejected {s}");
        }
        // Hex / unicode escapes.
        assert!(find_legacy_string_escape("\"\\x41\"").is_none());
        assert!(find_legacy_string_escape("\"\\u0041\"").is_none());
        assert!(find_legacy_string_escape("\"\\u{41}\"").is_none());
        // Escaped backslash — must not be treated as a fresh escape.
        assert!(find_legacy_string_escape("\"\\\\1\"").is_none());
        assert!(find_legacy_string_escape("\"\\\\\"").is_none());
        // Quoted regular text.
        assert!(find_legacy_string_escape("\"hello world\"").is_none());
        assert!(find_legacy_string_escape("''").is_none());
    }
}
