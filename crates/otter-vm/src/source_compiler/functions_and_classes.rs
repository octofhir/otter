//! Function-, arrow-, and class-expression compilation: inner
//! `FunctionCompiler` spawning for nested scopes, capture/upvalue wiring,
//! name-inference for anonymous expressions, and class-expression
//! delegation to `classes::compile_class_body`.
//!
//! Spec: ECMA-262 §15.2 (FunctionExpression), §15.3 (ArrowFunction),
//! §15.7 (ClassExpression), §13.2.5.5 (NamedEvaluation).

use super::ast::{expected_function_length, extract_function_params, extract_function_params_from_formal};
use super::module_compiler::{FunctionIdentity, ModuleCompiler};
use super::shared::{FunctionCompiler, FunctionKind, ValueLocation};
use super::*;

impl<'a> FunctionCompiler<'a> {
    pub(super) fn compile_function_expression(
        &mut self,
        function: &Function<'_>,
        inferred_name: Option<&str>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // §12.1.1 / §14.1.1 — In strict mode, `yield` and `let` cannot
        // be used as function binding identifiers. oxc doesn't enforce this.
        if self.strict_mode
            && let Some(id) = &function.id
        {
            let name = id.name.as_str();
            if name == "yield" || name == "let" {
                return Err(SourceLoweringError::EarlyError(format!(
                    "'{name}' is not allowed as a function name in strict mode"
                )));
            }
        }
        let fn_name = function.id.as_ref().map(|id| id.name.to_string());
        let public_name = fn_name
            .clone()
            .or_else(|| inferred_name.map(ToOwned::to_owned));

        let reserved = module.reserve_function();
        let params = extract_function_params(function)?;
        // Propagate private name context to inner function expressions.
        let saved_private_ctx = module.pending_has_class_private_context;
        module.pending_has_class_private_context = self.has_class_private_context;
        let compiled = module.compile_function_from_statements(
            reserved,
            FunctionIdentity {
                debug_name: public_name.clone().or_else(|| {
                    self.function_name
                        .as_ref()
                        .map(|name| format!("{name}::<anonymous>"))
                }),
                self_binding_name: fn_name,
                length: expected_function_length(&params),
            },
            function
                .body
                .as_ref()
                .map(|body| body.statements.as_slice())
                .ok_or_else(|| {
                    SourceLoweringError::Unsupported(
                        "function expressions without bodies".to_string(),
                    )
                })?,
            &params,
            if function.generator && function.r#async {
                FunctionKind::AsyncGenerator
            } else if function.generator {
                FunctionKind::Generator
            } else if function.r#async {
                FunctionKind::Async
            } else {
                FunctionKind::Ordinary
            },
            self.parent_scopes_for_child(),
            self.strict_mode
                || super::ast::has_use_strict_directive(
                    function
                        .body
                        .as_ref()
                        .map(|body| body.directives.as_slice())
                        .unwrap_or(&[]),
                ),
        )?;
        module.pending_has_class_private_context = saved_private_ctx;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        if function.generator && function.r#async {
            self.emit_new_closure_async_generator(destination, reserved, &compiled.captures)?;
        } else if function.generator {
            self.emit_new_closure_generator(destination, reserved, &compiled.captures)?;
        } else if function.r#async {
            self.emit_new_closure_async(destination, reserved, &compiled.captures)?;
        } else {
            self.emit_new_closure(destination, reserved, &compiled.captures)?;
        }
        // Propagate class_id to inner closures for private field access.
        if self.has_class_private_context {
            self.emit_copy_class_id_from_current(destination);
        }
        Ok(ValueLocation::temp(destination))
    }

    pub(super) fn compile_arrow_function_expression(
        &mut self,
        arrow: &oxc_ast::ast::ArrowFunctionExpression<'_>,
        inferred_name: Option<&str>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let public_name = inferred_name.map(ToOwned::to_owned);
        let reserved = module.reserve_function();
        let params = extract_function_params_from_formal(&arrow.params)?;
        let arrow_kind = if arrow.r#async {
            FunctionKind::AsyncArrow
        } else {
            FunctionKind::Arrow
        };

        // §15.3 ArrowFunction — inherits `this`, `super`, `new.target` from
        // the enclosing function. Propagate derived-constructor status so the
        // child FC allows `super()` compilation and the runtime can resolve
        // the enclosing constructor context.
        let saved_derived = module.pending_is_derived_constructor;
        module.pending_is_derived_constructor = self.is_derived_constructor;
        let saved_private_ctx = module.pending_has_class_private_context;
        module.pending_has_class_private_context = self.has_class_private_context;

        let compiled = if arrow.expression {
            let body_statements = &arrow.body.statements;
            let expression = match body_statements.first() {
                Some(AstStatement::ExpressionStatement(expr_stmt)) => &expr_stmt.expression,
                _ => {
                    return Err(SourceLoweringError::Unsupported(
                        "arrow expression body without expression statement".to_string(),
                    ));
                }
            };
            module.compile_function_from_expression(
                reserved,
                FunctionIdentity {
                    debug_name: public_name.clone().or_else(|| {
                        self.function_name
                            .as_ref()
                            .map(|name| format!("{name}::<arrow>"))
                    }),
                    self_binding_name: None,
                    length: expected_function_length(&params),
                },
                expression,
                &params,
                arrow_kind,
                self.parent_scopes_for_child(),
                self.strict_mode,
            )?
        } else {
            module.compile_function_from_statements(
                reserved,
                FunctionIdentity {
                    debug_name: public_name.clone().or_else(|| {
                        self.function_name
                            .as_ref()
                            .map(|name| format!("{name}::<arrow>"))
                    }),
                    self_binding_name: None,
                    length: expected_function_length(&params),
                },
                &arrow.body.statements,
                &params,
                arrow_kind,
                self.parent_scopes_for_child(),
                self.strict_mode
                    || super::ast::has_use_strict_directive(arrow.body.directives.as_slice()),
            )?
        };
        module.pending_is_derived_constructor = saved_derived;
        module.pending_has_class_private_context = saved_private_ctx;
        module.set_function(reserved, compiled.function);

        let destination = self.alloc_temp();
        if arrow.r#async {
            self.emit_new_closure_async_arrow(destination, reserved, &compiled.captures)?;
        } else {
            self.emit_new_closure_arrow(destination, reserved, &compiled.captures)?;
        }
        // Propagate class_id to inner closures for private field access.
        if self.has_class_private_context {
            self.emit_copy_class_id_from_current(destination);
        }
        Ok(ValueLocation::temp(destination))
    }

    /// §15.7 ClassExpression — `let x = class [Name] { ... }`
    /// Spec: <https://tc39.es/ecma262/#sec-class-definitions-runtime-semantics-evaluation>
    pub(super) fn compile_class_expression(
        &mut self,
        class: &oxc_ast::ast::Class<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        self.compile_class_expression_with_name(class, None, module)
    }

    /// §13.2.5.5 NamedEvaluation + §15.7 ClassExpression.
    ///
    /// When an anonymous class expression occurs in a NamedEvaluation
    /// context (e.g. `var E = class {}` or `obj.x = class {}`), the class
    /// constructor's `.name` should reflect the contextual binding name
    /// instead of the `"anonymous"` placeholder.
    pub(super) fn compile_class_expression_with_name(
        &mut self,
        class: &oxc_ast::ast::Class<'_>,
        inferred_name: Option<&str>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let class_name = match class.id.as_ref() {
            Some(id) => id.name.as_str(),
            None => inferred_name.unwrap_or(""),
        };
        self.compile_class_body(class, class_name, module)
    }
}
