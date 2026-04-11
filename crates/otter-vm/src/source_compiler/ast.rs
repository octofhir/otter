//! AST validators and pre-compile passes: early errors (strict-mode identifier
//! rules, duplicate bindings), hoisting collection (var / function / class),
//! private-name scope checks, parameter-info extraction, and test262 feature
//! recognizers (`is_test262_failure_throw`, `is_test262_assert_same_value_call`).

use super::*;
use oxc_ast::ast::Directive;
use oxc_ast_visit::{Visit, walk};

/// Represents a single function parameter, possibly with a default value.
pub(super) struct ParamInfo<'a> {
    pub(super) pattern: &'a BindingPattern<'a>,
    pub(super) default: Option<&'a Expression<'a>>,
    pub(super) is_rest: bool,
}

pub(super) fn expected_function_length(params: &[ParamInfo<'_>]) -> u16 {
    u16::try_from(
        params
            .iter()
            .take_while(|param| !param.is_rest && param.default.is_none())
            .count(),
    )
    .unwrap_or(u16::MAX)
}

pub(super) fn has_use_strict_directive(directives: &[Directive<'_>]) -> bool {
    directives
        .iter()
        .any(|directive| directive.directive == "use strict")
}

/// §8.2.2 IsSimpleParameterList — returns `true` when FormalParameters is a
/// list of plain `BindingIdentifier` elements with no defaults, no rest
/// parameter, and no destructuring patterns. Used by §15.2.1.1 and friends to
/// reject a `"use strict"` directive whose enclosing function has a
/// non-simple parameter list.
///
/// Spec: <https://tc39.es/ecma262/#sec-static-semantics-issimpleparameterlist>
pub(super) fn is_simple_parameter_list(params: &[ParamInfo<'_>]) -> bool {
    params.iter().all(|param| {
        !param.is_rest
            && param.default.is_none()
            && matches!(param.pattern, BindingPattern::BindingIdentifier(_))
    })
}

/// §15.7.14 / §8.3 AllPrivateNamesValid walker.
///
/// Visits every expression/statement in a subtree and, for each
/// `PrivateFieldExpression` or `PrivateInExpression` node, checks that
/// its name is valid given the caller-supplied `is_declared` predicate
/// (current class's private bound names union with the enclosing
/// lexical class chain). Stops descending into nested `ClassDeclaration`
/// / `ClassExpression` nodes because they validate on their own.
pub(super) struct PrivateNameValidator<'a> {
    pub is_declared: &'a dyn Fn(&str) -> bool,
    pub error: Option<SourceLoweringError>,
}

impl<'a, 'ast> Visit<'ast> for PrivateNameValidator<'a> {
    fn visit_private_field_expression(&mut self, it: &oxc_ast::ast::PrivateFieldExpression<'ast>) {
        if self.error.is_some() {
            return;
        }
        let name = it.field.name.as_str();
        if !(self.is_declared)(name) {
            self.error = Some(SourceLoweringError::EarlyError(format!(
                "Private name #{name} is not defined"
            )));
            return;
        }
        walk::walk_expression(self, &it.object);
    }

    fn visit_private_in_expression(&mut self, it: &oxc_ast::ast::PrivateInExpression<'ast>) {
        if self.error.is_some() {
            return;
        }
        let name = it.left.name.as_str();
        if !(self.is_declared)(name) {
            self.error = Some(SourceLoweringError::EarlyError(format!(
                "Private name #{name} is not defined"
            )));
            return;
        }
        walk::walk_expression(self, &it.right);
    }

    // Nested classes have their own lexical private environment; let the
    // recursive compile_class_body call validate them.
    fn visit_class(&mut self, _it: &oxc_ast::ast::Class<'ast>) {}
}

pub(super) fn check_expression_private_refs(
    expr: &Expression<'_>,
    is_declared: &dyn Fn(&str) -> bool,
) -> Result<(), SourceLoweringError> {
    let mut validator = PrivateNameValidator {
        is_declared,
        error: None,
    };
    walk::walk_expression(&mut validator, expr);
    match validator.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub(super) fn check_statement_private_refs(
    stmt: &AstStatement<'_>,
    is_declared: &dyn Fn(&str) -> bool,
) -> Result<(), SourceLoweringError> {
    let mut validator = PrivateNameValidator {
        is_declared,
        error: None,
    };
    walk::walk_statement(&mut validator, stmt);
    match validator.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

// ── ContainsArguments / Contains SuperCall check for field initializers ──────
//
// §15.7 Static Semantics: Early Errors for FieldDefinition:
//   - It is a Syntax Error if ContainsArguments of Initializer is true.
//   - It is a Syntax Error if Initializer Contains SuperCall is true.
//
// ContainsArguments recurses into arrow functions but NOT into regular
// function/generator/async expressions (they have their own `arguments`).
// Contains SuperCall similarly does NOT recurse into regular functions
// (but DOES recurse into arrow functions since they inherit `super`).
//
// Spec: <https://tc39.es/ecma262/#sec-static-semantics-containsarguments>
//       <https://tc39.es/ecma262/#sec-class-definitions-static-semantics-early-errors>
struct FieldInitializerValidator {
    error: Option<SourceLoweringError>,
}

impl<'ast> Visit<'ast> for FieldInitializerValidator {
    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'ast>) {
        if self.error.is_some() {
            return;
        }
        if it.name == "arguments" {
            self.error = Some(SourceLoweringError::EarlyError(
                "'arguments' is not allowed in class field initializer or computed property name"
                    .to_string(),
            ));
        }
    }

    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'ast>) {
        if self.error.is_some() {
            return;
        }
        if matches!(&it.callee, Expression::Super(_)) {
            self.error = Some(SourceLoweringError::EarlyError(
                "'super()' is not allowed in class field initializer".to_string(),
            ));
            return;
        }
        walk::walk_call_expression(self, it);
    }

    // Arrow functions inherit `arguments` and `super` from enclosing scope,
    // so we DO recurse into them — the default walk does this automatically.

    // Regular function expressions have their own `arguments` and `super`
    // scope — do NOT recurse into them.
    fn visit_function(
        &mut self,
        _it: &oxc_ast::ast::Function<'ast>,
        _flags: oxc_semantic::ScopeFlags,
    ) {
        // Skip — regular functions have their own arguments/super scope.
    }

    // Nested classes validate on their own.
    fn visit_class(&mut self, _it: &oxc_ast::ast::Class<'ast>) {}
}

/// Check a class field initializer for forbidden `arguments` and `super()`.
pub(super) fn check_field_initializer(expr: &Expression<'_>) -> Result<(), SourceLoweringError> {
    let mut validator = FieldInitializerValidator { error: None };
    walk::walk_expression(&mut validator, expr);
    match validator.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

// ── Eval-in-field-initializer validator ──────────────────────────────────
// §B.3.5.2 Additional Early Error Rules for Eval Inside Initializer:
// eval'd code in a field initializer must NOT contain `arguments`,
// `new.target`, or `super()`. Same recursion rules as FieldInitializerValidator
// but with the additional `new.target` restriction.
// Spec: <https://tc39.es/ecma262/#sec-performeval-rules-in-initializer>
struct EvalFieldInitializerValidator {
    error: Option<SourceLoweringError>,
}

impl<'ast> Visit<'ast> for EvalFieldInitializerValidator {
    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'ast>) {
        if self.error.is_some() {
            return;
        }
        if it.name == "arguments" {
            self.error = Some(SourceLoweringError::EarlyError(
                "'arguments' is not allowed in class field initializer or computed property name"
                    .to_string(),
            ));
        }
    }

    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'ast>) {
        if self.error.is_some() {
            return;
        }
        if matches!(&it.callee, Expression::Super(_)) {
            self.error = Some(SourceLoweringError::EarlyError(
                "'super()' is not allowed in class field initializer".to_string(),
            ));
            return;
        }
        walk::walk_call_expression(self, it);
    }

    // NOTE: `new.target` in direct eval inside field initializer is ALLOWED
    // (evaluates to undefined). Only `arguments` and `super()` are restricted.
    // §B.3.5.2: "The remaining eval rules apply as outside a constructor,
    // inside a method, and inside a function."

    fn visit_function(
        &mut self,
        _it: &oxc_ast::ast::Function<'ast>,
        _flags: oxc_semantic::ScopeFlags,
    ) {
    }

    fn visit_class(&mut self, _it: &oxc_ast::ast::Class<'ast>) {}
}

/// Check eval'd code inside a field initializer for forbidden constructs.
/// §B.3.5.2 Additional Early Error Rules for Eval Inside Initializer.
/// Spec: <https://tc39.es/ecma262/#sec-performeval-rules-in-initializer>
pub(crate) fn check_eval_field_initializer_program(
    program: &oxc_ast::ast::Program<'_>,
) -> Result<(), SourceLoweringError> {
    let mut validator = EvalFieldInitializerValidator { error: None };
    for stmt in &program.body {
        walk::walk_statement(&mut validator, stmt);
        if validator.error.is_some() {
            break;
        }
    }
    match validator.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub(super) fn identifier_name_for_parameter_pattern<'a>(
    pattern: &'a BindingPattern<'a>,
) -> Option<&'a str> {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => Some(identifier.name.as_str()),
        BindingPattern::ObjectPattern(_)
        | BindingPattern::ArrayPattern(_)
        | BindingPattern::AssignmentPattern(_) => None,
    }
}

pub(super) fn collect_binding_identifier_names(
    pattern: &BindingPattern<'_>,
    names: &mut Vec<String>,
) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            names.push(identifier.name.to_string());
        }
        BindingPattern::AssignmentPattern(assignment) => {
            collect_binding_identifier_names(&assignment.left, names);
        }
        BindingPattern::ObjectPattern(object_pattern) => {
            for property in &object_pattern.properties {
                collect_binding_identifier_names(&property.value, names);
            }
            if let Some(rest) = &object_pattern.rest {
                collect_binding_identifier_names(&rest.argument, names);
            }
        }
        BindingPattern::ArrayPattern(array_pattern) => {
            for element in array_pattern.elements.iter().flatten() {
                collect_binding_identifier_names(element, names);
            }
            if let Some(rest) = &array_pattern.rest {
                collect_binding_identifier_names(&rest.argument, names);
            }
        }
    }
}

pub(super) fn collect_var_names(statements: &[AstStatement<'_>]) -> Vec<String> {
    let mut names = Vec::new();
    for statement in statements {
        collect_var_names_from_statement(statement, &mut names);
    }
    names
}

fn collect_var_names_from_statement(statement: &AstStatement<'_>, names: &mut Vec<String>) {
    match statement {
        AstStatement::VariableDeclaration(declaration)
            if declaration.kind == VariableDeclarationKind::Var =>
        {
            for declarator in &declaration.declarations {
                if let BindingPattern::BindingIdentifier(identifier) = &declarator.id
                    && !names
                        .iter()
                        .any(|existing| existing == identifier.name.as_str())
                {
                    names.push(identifier.name.to_string());
                }
            }
        }
        // §16.2.3 — `export var x = 1` hoists the var declaration.
        AstStatement::ExportNamedDeclaration(export) => {
            if let Some(oxc_ast::ast::Declaration::VariableDeclaration(declaration)) =
                &export.declaration
                && declaration.kind == VariableDeclarationKind::Var
            {
                for declarator in &declaration.declarations {
                    if let BindingPattern::BindingIdentifier(identifier) = &declarator.id
                        && !names
                            .iter()
                            .any(|existing| existing == identifier.name.as_str())
                    {
                        names.push(identifier.name.to_string());
                    }
                }
            }
        }
        AstStatement::BlockStatement(block) => {
            for statement in &block.body {
                collect_var_names_from_statement(statement, names);
            }
        }
        AstStatement::IfStatement(if_statement) => {
            collect_var_names_from_statement(&if_statement.consequent, names);
            if let Some(alternate) = &if_statement.alternate {
                collect_var_names_from_statement(alternate, names);
            }
        }
        AstStatement::WhileStatement(while_statement) => {
            collect_var_names_from_statement(&while_statement.body, names);
        }
        AstStatement::DoWhileStatement(do_while_statement) => {
            collect_var_names_from_statement(&do_while_statement.body, names);
        }
        AstStatement::ForStatement(for_statement) => {
            if let Some(oxc_ast::ast::ForStatementInit::VariableDeclaration(declaration)) =
                &for_statement.init
                && declaration.kind == VariableDeclarationKind::Var
            {
                for declarator in &declaration.declarations {
                    if let BindingPattern::BindingIdentifier(identifier) = &declarator.id
                        && !names
                            .iter()
                            .any(|existing| existing == identifier.name.as_str())
                    {
                        names.push(identifier.name.to_string());
                    }
                }
            }
            collect_var_names_from_statement(&for_statement.body, names);
        }
        AstStatement::ForOfStatement(for_of_statement) => {
            if let ForStatementLeft::VariableDeclaration(declaration) = &for_of_statement.left
                && declaration.kind == VariableDeclarationKind::Var
            {
                for declarator in &declaration.declarations {
                    if let BindingPattern::BindingIdentifier(identifier) = &declarator.id
                        && !names
                            .iter()
                            .any(|existing| existing == identifier.name.as_str())
                    {
                        names.push(identifier.name.to_string());
                    }
                }
            }
            collect_var_names_from_statement(&for_of_statement.body, names);
        }
        AstStatement::ForInStatement(for_in_statement) => {
            if let ForStatementLeft::VariableDeclaration(declaration) = &for_in_statement.left
                && declaration.kind == VariableDeclarationKind::Var
            {
                for declarator in &declaration.declarations {
                    if let BindingPattern::BindingIdentifier(identifier) = &declarator.id
                        && !names
                            .iter()
                            .any(|existing| existing == identifier.name.as_str())
                    {
                        names.push(identifier.name.to_string());
                    }
                }
            }
            collect_var_names_from_statement(&for_in_statement.body, names);
        }
        AstStatement::LabeledStatement(labeled) => {
            collect_var_names_from_statement(&labeled.body, names);
        }
        AstStatement::TryStatement(try_statement) => {
            for statement in &try_statement.block.body {
                collect_var_names_from_statement(statement, names);
            }
            if let Some(handler) = &try_statement.handler {
                for statement in &handler.body.body {
                    collect_var_names_from_statement(statement, names);
                }
            }
            if let Some(finalizer) = &try_statement.finalizer {
                for statement in &finalizer.body {
                    collect_var_names_from_statement(statement, names);
                }
            }
        }
        _ => {}
    }
}

/// §8.1.1.2 TopLevelLexicallyDeclaredNames — names introduced by `let`,
/// `const`, or `class` declarations at the top level of a function body or
/// script body. Does NOT recurse into nested blocks/loops/try/etc., because
/// `let`/`const`/`class` are block-scoped — only the immediate body counts.
///
/// Used during `predeclare_function_scope` so that hoisted nested function
/// declarations can correctly capture top-level lexical bindings via the
/// closure scope chain (rather than falling back to a runtime global lookup
/// that misses script-level `let` bindings).
pub(super) fn collect_top_level_lexical_names(statements: &[AstStatement<'_>]) -> Vec<String> {
    fn push_unique(names: &mut Vec<String>, name: &str) {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }

    let mut names: Vec<String> = Vec::new();
    for statement in statements {
        match statement {
            AstStatement::VariableDeclaration(declaration)
                if matches!(
                    declaration.kind,
                    VariableDeclarationKind::Let | VariableDeclarationKind::Const
                ) =>
            {
                let mut bound = Vec::new();
                for declarator in &declaration.declarations {
                    collect_binding_identifier_names(&declarator.id, &mut bound);
                }
                for name in bound {
                    push_unique(&mut names, &name);
                }
            }
            AstStatement::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    push_unique(&mut names, id.name.as_str());
                }
            }
            AstStatement::ExportNamedDeclaration(export) => match &export.declaration {
                Some(oxc_ast::ast::Declaration::VariableDeclaration(declaration))
                    if matches!(
                        declaration.kind,
                        VariableDeclarationKind::Let | VariableDeclarationKind::Const
                    ) =>
                {
                    let mut bound = Vec::new();
                    for declarator in &declaration.declarations {
                        collect_binding_identifier_names(&declarator.id, &mut bound);
                    }
                    for name in bound {
                        push_unique(&mut names, &name);
                    }
                }
                Some(oxc_ast::ast::Declaration::ClassDeclaration(class)) => {
                    if let Some(id) = &class.id {
                        push_unique(&mut names, id.name.as_str());
                    }
                }
                _ => {}
            },
            AstStatement::ExportDefaultDeclaration(export) => {
                if let oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(class) =
                    &export.declaration
                    && let Some(id) = &class.id
                {
                    push_unique(&mut names, id.name.as_str());
                }
            }
            _ => {}
        }
    }
    names
}

pub(super) fn collect_function_declarations<'a>(
    statements: &'a [AstStatement<'a>],
    functions: &mut Vec<&'a Function<'a>>,
) {
    for statement in statements {
        collect_function_declarations_from_statement(statement, functions);
    }
}

fn collect_function_declarations_from_statement<'a>(
    statement: &'a AstStatement<'a>,
    functions: &mut Vec<&'a Function<'a>>,
) {
    match statement {
        AstStatement::FunctionDeclaration(function) => functions.push(function),
        // §16.2.3 — `export function f() {}` hoists the function declaration.
        AstStatement::ExportNamedDeclaration(export) => {
            if let Some(oxc_ast::ast::Declaration::FunctionDeclaration(function)) =
                &export.declaration
            {
                functions.push(function);
            }
        }
        // §16.2.3 — `export default function f() {}` hoists the named function.
        AstStatement::ExportDefaultDeclaration(export) => {
            if let oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(function) =
                &export.declaration
                && function.id.is_some()
            {
                functions.push(function);
            }
        }
        AstStatement::BlockStatement(block) => {
            for statement in &block.body {
                collect_function_declarations_from_statement(statement, functions);
            }
        }
        AstStatement::IfStatement(if_statement) => {
            collect_function_declarations_from_statement(&if_statement.consequent, functions);
            if let Some(alternate) = &if_statement.alternate {
                collect_function_declarations_from_statement(alternate, functions);
            }
        }
        AstStatement::WhileStatement(while_statement) => {
            collect_function_declarations_from_statement(&while_statement.body, functions);
        }
        AstStatement::DoWhileStatement(do_while_statement) => {
            collect_function_declarations_from_statement(&do_while_statement.body, functions);
        }
        AstStatement::ForStatement(for_statement) => {
            collect_function_declarations_from_statement(&for_statement.body, functions);
        }
        AstStatement::ForOfStatement(for_of_statement) => {
            collect_function_declarations_from_statement(&for_of_statement.body, functions);
        }
        AstStatement::ForInStatement(for_in_statement) => {
            collect_function_declarations_from_statement(&for_in_statement.body, functions);
        }
        AstStatement::LabeledStatement(labeled) => {
            collect_function_declarations_from_statement(&labeled.body, functions);
        }
        AstStatement::TryStatement(try_statement) => {
            for statement in &try_statement.block.body {
                collect_function_declarations_from_statement(statement, functions);
            }
            if let Some(handler) = &try_statement.handler {
                for statement in &handler.body.body {
                    collect_function_declarations_from_statement(statement, functions);
                }
            }
            if let Some(finalizer) = &try_statement.finalizer {
                for statement in &finalizer.body {
                    collect_function_declarations_from_statement(statement, functions);
                }
            }
        }
        _ => {}
    }
}

pub(super) fn extract_function_params<'a>(
    function: &'a Function<'a>,
) -> Result<Vec<ParamInfo<'a>>, SourceLoweringError> {
    extract_function_params_from_formal(&function.params)
}

pub(super) fn extract_function_params_from_formal<'a>(
    params: &'a oxc_ast::ast::FormalParameters<'a>,
) -> Result<Vec<ParamInfo<'a>>, SourceLoweringError> {
    let mut result = Vec::new();
    for param in &params.items {
        match &param.pattern {
            BindingPattern::BindingIdentifier(_) => {
                result.push(ParamInfo {
                    pattern: &param.pattern,
                    default: param.initializer.as_deref(),
                    is_rest: false,
                });
            }
            BindingPattern::ObjectPattern(_) | BindingPattern::ArrayPattern(_) => {
                result.push(ParamInfo {
                    pattern: &param.pattern,
                    default: param.initializer.as_deref(),
                    is_rest: false,
                });
            }
            BindingPattern::AssignmentPattern(_) => {
                return Err(SourceLoweringError::Unsupported(
                    "assignment pattern parameters".to_string(),
                ));
            }
        }
    }
    if let Some(rest) = &params.rest {
        result.push(ParamInfo {
            pattern: &rest.rest.argument,
            default: None,
            is_rest: true,
        });
    }
    Ok(result)
}

pub(super) fn non_computed_property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.to_string()),
        PropertyKey::Identifier(identifier) => Some(identifier.name.to_string()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.to_string()),
        PropertyKey::NumericLiteral(literal) => {
            Some(crate::abstract_ops::ecma_number_to_string(literal.value))
        }
        PropertyKey::BooleanLiteral(literal) => Some(if literal.value {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        PropertyKey::NullLiteral(_) => Some("null".to_string()),
        _ => None,
    }
}

pub(super) fn inferred_name_for_binding_pattern<'a>(
    pattern: &'a BindingPattern<'a>,
) -> Option<&'a str> {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => Some(identifier.name.as_str()),
        BindingPattern::AssignmentPattern(assignment) => {
            inferred_name_for_binding_pattern(&assignment.left)
        }
        BindingPattern::ObjectPattern(_) | BindingPattern::ArrayPattern(_) => None,
    }
}

pub(super) fn inferred_name_for_assignment_target<'a>(
    target: &'a AssignmentTarget<'a>,
) -> Option<&'a str> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(identifier) => Some(identifier.name.as_str()),
        AssignmentTarget::ArrayAssignmentTarget(_)
        | AssignmentTarget::ObjectAssignmentTarget(_)
        | AssignmentTarget::ComputedMemberExpression(_)
        | AssignmentTarget::StaticMemberExpression(_)
        | AssignmentTarget::PrivateFieldExpression(_)
        | AssignmentTarget::TSAsExpression(_)
        | AssignmentTarget::TSSatisfiesExpression(_)
        | AssignmentTarget::TSNonNullExpression(_)
        | AssignmentTarget::TSTypeAssertion(_) => None,
    }
}

pub(super) fn is_test262_failure_throw(expression: &Expression<'_>) -> bool {
    let Expression::NewExpression(new_expression) = expression else {
        return false;
    };
    let Expression::Identifier(identifier) = &new_expression.callee else {
        return false;
    };
    identifier.name == "Test262Error"
}

pub(super) fn is_test262_assert_same_value_call(call: &oxc_ast::ast::CallExpression<'_>) -> bool {
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return false;
    };
    let Expression::Identifier(object) = &member.object else {
        return false;
    };
    object.name == "assert" && member.property.name == "sameValue"
}
