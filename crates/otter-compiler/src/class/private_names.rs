//! Private-name and direct-super validation for class bodies.
//!
//! # Contents
//! - [`validate_no_direct_super_in_methods`] - reject direct `super(...)` outside derived constructors.
//! - [`collect_class_private_bound`] - collect private names declared by a class body.
//! - [`private_name_in_scope`] - test private-name visibility through nested class scopes.
//! - [`validate_private_refs_in_expression`] - validate private references in expressions.
//! - [`validate_private_refs_in_function_body`] - validate private references in function bodies.
//! - [`validate_class_private_names_inner`] - recursive private-name validator with scope stack.
//!
//! # Invariants
//! - Heritage expressions are checked against the outer private environment.
//! - Nested class bodies push their own private-name scope before validating references.
//!
//! # See also
//! - [`super`]

use super::property_key_as_expression;
use crate::*;

/// §15.7.1 Class Definitions: Static Semantics: HasDirectSuper /
/// Early Errors. Walks each `ClassElement` in `body` and raises
/// `CompileError::Syntax` if any non-constructor method definition
/// or field initializer contains a direct `super(...)` call.
///
/// "Direct" matches the spec definition: only `SuperCall` nodes
/// reachable through arrow functions stay in scope; entering a
/// non-arrow function or a nested method definition resets the
/// super binding and is therefore transparent for this check.
pub(crate) fn validate_no_direct_super_in_methods(
    body: &oxc_ast::ast::ClassBody<'_>,
) -> Result<(), CompileError> {
    use oxc_ast_visit::Visit;
    struct SuperFinder {
        nested_function_depth: u32,
        found: Option<(u32, u32)>,
    }
    impl<'a> Visit<'a> for SuperFinder {
        fn visit_function(
            &mut self,
            it: &oxc_ast::ast::Function<'a>,
            flags: oxc_syntax::scope::ScopeFlags,
        ) {
            // Non-arrow function — new HomeObject scope; spec resets
            // HasDirectSuper across the boundary.
            self.nested_function_depth += 1;
            oxc_ast_visit::walk::walk_function(self, it, flags);
            self.nested_function_depth -= 1;
        }
        fn visit_method_definition(&mut self, it: &oxc_ast::ast::MethodDefinition<'a>) {
            // Inner-class methods carry their own HomeObject.
            self.nested_function_depth += 1;
            oxc_ast_visit::walk::walk_method_definition(self, it);
            self.nested_function_depth -= 1;
        }
        fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
            if self.nested_function_depth == 0
                && matches!(it.callee, oxc_ast::ast::Expression::Super(_))
                && self.found.is_none()
            {
                self.found = Some((it.span.start, it.span.end));
            }
            oxc_ast_visit::walk::walk_call_expression(self, it);
        }
    }
    for element in &body.body {
        match element {
            oxc_ast::ast::ClassElement::MethodDefinition(m)
                if !matches!(m.kind, oxc_ast::ast::MethodDefinitionKind::Constructor) =>
            {
                let mut finder = SuperFinder {
                    nested_function_depth: 0,
                    found: None,
                };
                // Walk the method body + parameter defaults directly
                // so the outermost function-scope is treated as the
                // method itself (depth 0) — nested non-arrow
                // functions / inner methods still bump depth via the
                // overrides on `Visit`.
                if let Some(body) = m.value.body.as_deref() {
                    for stmt in &body.statements {
                        finder.visit_statement(stmt);
                    }
                }
                for param in &m.value.params.items {
                    if let Some(init) = param.initializer.as_deref() {
                        finder.visit_expression(init);
                    }
                }
                if finder.found.is_some() {
                    return Err(CompileError::Syntax {
                        messages: vec![
                            "SyntaxError: 'super' call is only allowed in a derived-class constructor"
                                .to_string(),
                        ],
                        diagnostics: Vec::new(),
                    });
                }
            }
            oxc_ast::ast::ClassElement::PropertyDefinition(p) => {
                if let Some(init) = p.value.as_ref() {
                    let mut finder = SuperFinder {
                        nested_function_depth: 0,
                        found: None,
                    };
                    finder.visit_expression(init);
                    if let Some(_span) = finder.found {
                        return Err(CompileError::Syntax {
                            messages: vec![
                                "SyntaxError: 'super' call is only allowed in a derived-class constructor"
                                    .to_string(),
                            ],
                            diagnostics: Vec::new(),
                        });
                    }
                }
            }
            oxc_ast::ast::ClassElement::StaticBlock(b) => {
                let mut finder = SuperFinder {
                    nested_function_depth: 0,
                    found: None,
                };
                for stmt in &b.body {
                    finder.visit_statement(stmt);
                }
                if let Some(_span) = finder.found {
                    return Err(CompileError::Syntax {
                        messages: vec![
                            "SyntaxError: 'super' call is only allowed in a derived-class constructor"
                                .to_string(),
                        ],
                        diagnostics: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn collect_class_private_bound(body: &oxc_ast::ast::ClassBody<'_>) -> Vec<String> {
    // Declaration order, deduped — the field-init path indexes the
    // per-class private-symbol array by this order.
    let mut seen = std::collections::HashSet::new();
    let mut names: Vec<String> = Vec::new();
    let mut push = |n: String| {
        if seen.insert(n.clone()) {
            names.push(n);
        }
    };
    for element in &body.body {
        match element {
            oxc_ast::ast::ClassElement::MethodDefinition(m) => {
                if let oxc_ast::ast::PropertyKey::PrivateIdentifier(p) = &m.key {
                    push(p.name.to_string());
                }
            }
            oxc_ast::ast::ClassElement::PropertyDefinition(p) => {
                if let oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) = &p.key {
                    push(pid.name.to_string());
                }
            }
            oxc_ast::ast::ClassElement::AccessorProperty(a) => {
                if let oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) = &a.key {
                    push(pid.name.to_string());
                }
            }
            _ => {}
        }
    }
    names
}

pub(crate) fn private_name_in_scope(
    scopes: &[std::collections::HashSet<String>],
    name: &str,
) -> bool {
    scopes.iter().rev().any(|s| s.contains(name))
}

pub(crate) fn validate_private_refs_in_expression(
    expr: &oxc_ast::ast::Expression<'_>,
    scopes: &mut Vec<std::collections::HashSet<String>>,
) -> Result<(), CompileError> {
    use oxc_ast_visit::Visit;
    // Custom visitor so we can re-enter nested classes (which mutate
    // the scope stack) without losing the validator's invariants.
    struct PrivateRefFinder<'s> {
        scopes: &'s mut Vec<std::collections::HashSet<String>>,
        err: Option<CompileError>,
    }
    impl<'a, 's> Visit<'a> for PrivateRefFinder<'s> {
        fn visit_private_field_expression(
            &mut self,
            it: &oxc_ast::ast::PrivateFieldExpression<'a>,
        ) {
            if self.err.is_some() {
                return;
            }
            let name = it.field.name.as_str();
            if !private_name_in_scope(self.scopes, name) {
                self.err = Some(CompileError::Syntax {
                    messages: vec![format!("SyntaxError: undeclared private name '#{name}'")],
                    diagnostics: Vec::new(),
                });
                return;
            }
            oxc_ast_visit::walk::walk_private_field_expression(self, it);
        }
        fn visit_private_in_expression(&mut self, it: &oxc_ast::ast::PrivateInExpression<'a>) {
            if self.err.is_some() {
                return;
            }
            let name = it.left.name.as_str();
            if !private_name_in_scope(self.scopes, name) {
                self.err = Some(CompileError::Syntax {
                    messages: vec![format!("SyntaxError: undeclared private name '#{name}'")],
                    diagnostics: Vec::new(),
                });
                return;
            }
            oxc_ast_visit::walk::walk_private_in_expression(self, it);
        }
        fn visit_class(&mut self, it: &oxc_ast::ast::Class<'a>) {
            if self.err.is_some() {
                return;
            }
            if let Err(e) = validate_class_private_names_inner(it, self.scopes) {
                self.err = Some(e);
            }
        }
    }
    let mut finder = PrivateRefFinder { scopes, err: None };
    finder.visit_expression(expr);
    match finder.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub(crate) fn validate_private_refs_in_function_body(
    func: &oxc_ast::ast::Function<'_>,
    scopes: &mut Vec<std::collections::HashSet<String>>,
) -> Result<(), CompileError> {
    use oxc_ast_visit::Visit;
    struct PrivateRefFinder<'s> {
        scopes: &'s mut Vec<std::collections::HashSet<String>>,
        err: Option<CompileError>,
    }
    impl<'a, 's> Visit<'a> for PrivateRefFinder<'s> {
        fn visit_private_field_expression(
            &mut self,
            it: &oxc_ast::ast::PrivateFieldExpression<'a>,
        ) {
            if self.err.is_some() {
                return;
            }
            let name = it.field.name.as_str();
            if !private_name_in_scope(self.scopes, name) {
                self.err = Some(CompileError::Syntax {
                    messages: vec![format!("SyntaxError: undeclared private name '#{name}'")],
                    diagnostics: Vec::new(),
                });
                return;
            }
            oxc_ast_visit::walk::walk_private_field_expression(self, it);
        }
        fn visit_private_in_expression(&mut self, it: &oxc_ast::ast::PrivateInExpression<'a>) {
            if self.err.is_some() {
                return;
            }
            let name = it.left.name.as_str();
            if !private_name_in_scope(self.scopes, name) {
                self.err = Some(CompileError::Syntax {
                    messages: vec![format!("SyntaxError: undeclared private name '#{name}'")],
                    diagnostics: Vec::new(),
                });
                return;
            }
            oxc_ast_visit::walk::walk_private_in_expression(self, it);
        }
        fn visit_class(&mut self, it: &oxc_ast::ast::Class<'a>) {
            if self.err.is_some() {
                return;
            }
            if let Err(e) = validate_class_private_names_inner(it, self.scopes) {
                self.err = Some(e);
            }
        }
    }
    let mut finder = PrivateRefFinder { scopes, err: None };
    for param in &func.params.items {
        if let Some(init) = param.initializer.as_deref() {
            finder.visit_expression(init);
        }
    }
    if let Some(body) = func.body.as_deref() {
        for stmt in &body.statements {
            finder.visit_statement(stmt);
        }
    }
    match finder.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub(crate) fn validate_class_private_names_inner(
    class: &oxc_ast::ast::Class<'_>,
    scopes: &mut Vec<std::collections::HashSet<String>>,
) -> Result<(), CompileError> {
    // Heritage is evaluated under the outer private environment per
    // §15.7.14 step 10.b — push happens after the heritage walk.
    if let Some(parent) = &class.super_class {
        validate_private_refs_in_expression(parent, scopes)?;
    }
    let own = collect_class_private_bound(&class.body);
    scopes.push(own.into_iter().collect());
    let res = (|| -> Result<(), CompileError> {
        for element in &class.body.body {
            match element {
                oxc_ast::ast::ClassElement::MethodDefinition(m) => {
                    // Computed key expressions are evaluated in the
                    // class's private scope.
                    if let Some(e) = property_key_as_expression(&m.key) {
                        validate_private_refs_in_expression(e, scopes)?;
                    }
                    validate_private_refs_in_function_body(&m.value, scopes)?;
                }
                oxc_ast::ast::ClassElement::PropertyDefinition(p) => {
                    if let Some(e) = property_key_as_expression(&p.key) {
                        validate_private_refs_in_expression(e, scopes)?;
                    }
                    if let Some(init) = p.value.as_ref() {
                        validate_private_refs_in_expression(init, scopes)?;
                    }
                }
                oxc_ast::ast::ClassElement::AccessorProperty(a) => {
                    if let Some(e) = property_key_as_expression(&a.key) {
                        validate_private_refs_in_expression(e, scopes)?;
                    }
                    if let Some(init) = a.value.as_ref() {
                        validate_private_refs_in_expression(init, scopes)?;
                    }
                }
                oxc_ast::ast::ClassElement::StaticBlock(b) => {
                    use oxc_ast_visit::Visit;
                    struct PrivateRefFinder<'s> {
                        scopes: &'s mut Vec<std::collections::HashSet<String>>,
                        err: Option<CompileError>,
                    }
                    impl<'a, 's> Visit<'a> for PrivateRefFinder<'s> {
                        fn visit_private_field_expression(
                            &mut self,
                            it: &oxc_ast::ast::PrivateFieldExpression<'a>,
                        ) {
                            if self.err.is_some() {
                                return;
                            }
                            let name = it.field.name.as_str();
                            if !private_name_in_scope(self.scopes, name) {
                                self.err = Some(CompileError::Syntax {
                                    messages: vec![format!(
                                        "SyntaxError: undeclared private name '#{name}'"
                                    )],
                                    diagnostics: Vec::new(),
                                });
                                return;
                            }
                            oxc_ast_visit::walk::walk_private_field_expression(self, it);
                        }
                        fn visit_private_in_expression(
                            &mut self,
                            it: &oxc_ast::ast::PrivateInExpression<'a>,
                        ) {
                            if self.err.is_some() {
                                return;
                            }
                            let name = it.left.name.as_str();
                            if !private_name_in_scope(self.scopes, name) {
                                self.err = Some(CompileError::Syntax {
                                    messages: vec![format!(
                                        "SyntaxError: undeclared private name '#{name}'"
                                    )],
                                    diagnostics: Vec::new(),
                                });
                                return;
                            }
                            oxc_ast_visit::walk::walk_private_in_expression(self, it);
                        }
                        fn visit_class(&mut self, it: &oxc_ast::ast::Class<'a>) {
                            if self.err.is_some() {
                                return;
                            }
                            if let Err(e) = validate_class_private_names_inner(it, self.scopes) {
                                self.err = Some(e);
                            }
                        }
                    }
                    let mut finder = PrivateRefFinder { scopes, err: None };
                    for stmt in &b.body {
                        finder.visit_statement(stmt);
                    }
                    if let Some(e) = finder.err {
                        return Err(e);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    scopes.pop();
    res
}

/// Instance private METHOD / ACCESSOR names (§7.3.30 — these brand
/// the receiver; fields do not).
pub(crate) fn collect_class_private_instance_methods(
    body: &oxc_ast::ast::ClassBody<'_>,
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for element in &body.body {
        if let oxc_ast::ast::ClassElement::MethodDefinition(m) = element
            && !m.r#static
            && let oxc_ast::ast::PropertyKey::PrivateIdentifier(p) = &m.key
        {
            names.insert(p.name.to_string());
        }
    }
    names
}
