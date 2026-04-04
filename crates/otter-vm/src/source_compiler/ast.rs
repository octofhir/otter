use super::*;
use oxc_ast::ast::Directive;

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
        PropertyKey::NumericLiteral(literal) => Some(literal.value.to_string()),
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
