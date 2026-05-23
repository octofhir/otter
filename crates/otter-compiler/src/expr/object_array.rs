//! Object and array literal lowering.
//!
//! # Contents
//! - [`compile_object_literal`] — lowers object literals.
//! - [`compile_array_literal`] — lowers array literals.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::{ArrayExpression, ObjectExpression};

pub(crate) fn compile_array_literal(
    cx: &mut Compiler,
    arr: &ArrayExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (arr.span.start, arr.span.end);
    let has_spread = arr
        .elements
        .iter()
        .any(|el| matches!(el, oxc_ast::ast::ArrayExpressionElement::SpreadElement(_)));
    if !has_spread {
        let mut element_regs: Vec<u16> = Vec::with_capacity(arr.elements.len());
        for el in &arr.elements {
            match el {
                oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => {
                    unreachable!("spread excluded above")
                }
                oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                    // §10.4.2 ArrayExoticObject: emit the
                    // internal hole sentinel so the resulting
                    // dense slot stays distinguishable from
                    // explicit `undefined` for `in`,
                    // `Array.prototype` callbacks, and JSON
                    // serialisation.
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadHole, [Operand::Register(r)], span);
                    element_regs.push(r);
                }
                other => {
                    let expr = other.to_expression();
                    element_regs.push(compile_expr(cx, expr, span)?);
                }
            }
        }
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + element_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(element_regs.len() as u32));
        operands.extend(element_regs.into_iter().map(Operand::Register));
        cx.emit(Op::NewArray, operands, span);
        Ok(dst)
    } else {
        // Spread path: materialise an empty array, then
        // append each element (or each iterator step for
        // spread elements). Slightly less efficient than
        // the dense `NewArray` form, but only paid for
        // literals that actually contain `...`.
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::NewArray,
            [Operand::Register(dst), Operand::ConstIndex(0)],
            span,
        );
        for el in &arr.elements {
            match el {
                oxc_ast::ast::ArrayExpressionElement::SpreadElement(s) => {
                    let inner_span = (s.span.start, s.span.end);
                    emit_spread_into_array(cx, dst, &s.argument, inner_span)?;
                }
                oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                    // Spread path's hole branch: same hole
                    // sentinel as the dense `NewArray` form
                    // above. `Op::ArrayPush` simply forwards
                    // the register value into the body.
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadHole, [Operand::Register(r)], span);
                    cx.emit(
                        Op::ArrayPush,
                        [Operand::Register(dst), Operand::Register(r)],
                        span,
                    );
                }
                other => {
                    let expr = other.to_expression();
                    let r = compile_expr(cx, expr, span)?;
                    cx.emit(
                        Op::ArrayPush,
                        [Operand::Register(dst), Operand::Register(r)],
                        span,
                    );
                }
            }
        }
        Ok(dst)
    }
}

pub(crate) fn compile_object_literal(
    cx: &mut Compiler,
    obj: &ObjectExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (obj.span.start, obj.span.end);
    let dst = cx.alloc_scratch();
    cx.emit(Op::NewObject, [Operand::Register(dst)], span);

    // §13.2.5.5 PropertyDefinitionEvaluation — concise method, getter
    // and setter definitions in an object literal receive
    // [[HomeObject]] = the object being constructed, so any `super`
    // reference inside their bodies walks one hop up the object's
    // own [[Prototype]] chain. Install the synthetic `__class_home`
    // binding in a fresh scope so inner method bodies pick it up
    // through the standard upvalue walker — same mechanism the class
    // lowering uses (see `crate::class::SUPER_HOME_NAME`).
    // <https://tc39.es/ecma262/#sec-object-initializer-runtime-semantics-propertydefinitionevaluation>
    // <https://tc39.es/ecma262/#sec-makemethod>
    let needs_home = object_literal_uses_super_in_methods(obj);
    if needs_home {
        cx.enter_scope();
        let storage = cx.declare_captured_binding(crate::class::SUPER_HOME_NAME, true, span)?;
        cx.emit_store_storage(dst, storage, span);
        cx.mark_initialized(crate::class::SUPER_HOME_NAME);
    }
    for prop in &obj.properties {
        match prop {
            oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) => {
                let key_span = (p.span.start, p.span.end);
                // §13.2.5 Object Initializer — computed-key
                // properties (`{ [expr]: value }`) lower to
                // `Op::StoreElement` with the key value
                // computed at runtime. Static-key paths
                // keep the existing `Op::StoreProperty`
                // fast path.
                // <https://tc39.es/ecma262/#sec-object-initializer>
                let static_key_str = if p.computed {
                    None
                } else {
                    Some(match &p.key {
                        oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                            id.name.as_str().to_string()
                        }
                        oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
                        oxc_ast::ast::PropertyKey::NumericLiteral(lit) => lit.value.to_string(),
                        _ => {
                            return Err(CompileError::Unsupported {
                                node: "ObjectExpression: non-string property key".to_string(),
                                span: key_span,
                            });
                        }
                    })
                };
                if matches!(
                    p.kind,
                    oxc_ast::ast::PropertyKind::Get | oxc_ast::ast::PropertyKind::Set
                ) {
                    let key_reg =
                        match &static_key_str {
                            Some(key) => {
                                let r = cx.alloc_scratch();
                                let const_idx = cx.intern_string_constant(key);
                                cx.emit(
                                    Op::LoadString,
                                    [Operand::Register(r), Operand::ConstIndex(const_idx)],
                                    key_span,
                                );
                                r
                            }
                            None => {
                                let expr = p.key.as_expression().ok_or_else(|| {
                                    CompileError::Unsupported {
                                    node: "ObjectExpression: computed accessor key (non-expression)"
                                        .to_string(),
                                    span: key_span,
                                }
                                })?;
                                compile_expr(cx, expr, key_span)?
                            }
                        };
                    let function_reg = compile_expr(cx, &p.value, key_span)?;
                    let desc_reg = cx.alloc_scratch();
                    cx.emit(Op::NewObject, [Operand::Register(desc_reg)], key_span);
                    let accessor_key = match p.kind {
                        oxc_ast::ast::PropertyKind::Get => "get",
                        oxc_ast::ast::PropertyKind::Set => "set",
                        oxc_ast::ast::PropertyKind::Init => unreachable!(),
                    };
                    let accessor_const = cx.intern_string_constant(accessor_key);
                    let store_scratch = cx.alloc_scratch();
                    cx.emit(
                        Op::StoreProperty,
                        vec![
                            Operand::Register(desc_reg),
                            Operand::ConstIndex(accessor_const),
                            Operand::Register(function_reg),
                            Operand::Register(store_scratch),
                        ],
                        key_span,
                    );
                    let true_reg = cx.alloc_scratch();
                    cx.emit(Op::LoadTrue, [Operand::Register(true_reg)], key_span);
                    for attr in ["enumerable", "configurable"] {
                        let attr_const = cx.intern_string_constant(attr);
                        let attr_scratch = cx.alloc_scratch();
                        cx.emit(
                            Op::StoreProperty,
                            vec![
                                Operand::Register(desc_reg),
                                Operand::ConstIndex(attr_const),
                                Operand::Register(true_reg),
                                Operand::Register(attr_scratch),
                            ],
                            key_span,
                        );
                    }
                    cx.emit(
                        Op::DefineOwnProperty,
                        [
                            Operand::Register(dst),
                            Operand::Register(key_reg),
                            Operand::Register(desc_reg),
                        ],
                        key_span,
                    );
                    continue;
                }
                if p.computed {
                    let key_reg =
                        match &p.key {
                            oxc_ast::ast::PropertyKey::StaticIdentifier(_)
                            | oxc_ast::ast::PropertyKey::StringLiteral(_) => {
                                // Even when the syntax is
                                // computed, oxc still preserves
                                // the literal — but we lower
                                // through the dynamic path so
                                // string / symbol identity
                                // observably matches spec.
                                let expr = p.key.as_expression().ok_or_else(|| {
                                    CompileError::Unsupported {
                                        node: "ObjectExpression: computed key (non-expression)"
                                            .to_string(),
                                        span: key_span,
                                    }
                                })?;
                                compile_expr(cx, expr, key_span)?
                            }
                            _ => {
                                let expr = p.key.as_expression().ok_or_else(|| {
                                    CompileError::Unsupported {
                                        node: "ObjectExpression: computed key (non-expression)"
                                            .to_string(),
                                        span: key_span,
                                    }
                                })?;
                                compile_expr(cx, expr, key_span)?
                            }
                        };
                    let value_reg = compile_expr(cx, &p.value, key_span)?;
                    cx.emit_store_element(dst, key_reg, value_reg, key_span);
                    continue;
                }
                let key_str = static_key_str.expect("non-computed key resolved above");
                let value_reg = compile_expr(cx, &p.value, key_span)?;
                let const_idx = cx.intern_string_constant(&key_str);
                let store_scratch = cx.alloc_scratch();
                cx.emit(
                    Op::StoreProperty,
                    vec![
                        Operand::Register(dst),
                        Operand::ConstIndex(const_idx),
                        Operand::Register(value_reg),
                        Operand::Register(store_scratch),
                    ],
                    key_span,
                );
            }
            // §13.2.5.5 PropertyDefinitionEvaluation —
            // `{ ...source }` copies enumerable own
            // properties from `source` onto the object
            // under construction via §7.3.31 CopyDataProperties.
            oxc_ast::ast::ObjectPropertyKind::SpreadProperty(s) => {
                let s_span = (s.span.start, s.span.end);
                let src = compile_expr(cx, &s.argument, s_span)?;
                cx.emit(
                    Op::CopyDataProperties,
                    [Operand::Register(dst), Operand::Register(src)],
                    s_span,
                );
            }
        }
    }
    if needs_home {
        cx.exit_scope();
    }
    Ok(dst)
}

/// Walks an object literal's method / getter / setter bodies looking
/// for a `super` reference that would resolve to the object's
/// [[HomeObject]]. Nested non-arrow functions and inner method
/// definitions reset the super binding per §15.4.4 MakeMethod /
/// §15.7.1, so we stop descending into them — arrow functions stay
/// transparent because their `super` resolves through the enclosing
/// method's home. Returns `true` if at least one method body or
/// parameter initializer needs the synthetic `__class_home` capture.
fn object_literal_uses_super_in_methods(obj: &ObjectExpression<'_>) -> bool {
    use oxc_ast_visit::Visit;

    struct SuperFinder {
        found: bool,
        nested_function_depth: u32,
    }
    impl<'a> Visit<'a> for SuperFinder {
        fn visit_function(
            &mut self,
            it: &oxc_ast::ast::Function<'a>,
            flags: oxc_syntax::scope::ScopeFlags,
        ) {
            self.nested_function_depth += 1;
            oxc_ast_visit::walk::walk_function(self, it, flags);
            self.nested_function_depth -= 1;
        }
        fn visit_method_definition(&mut self, it: &oxc_ast::ast::MethodDefinition<'a>) {
            self.nested_function_depth += 1;
            oxc_ast_visit::walk::walk_method_definition(self, it);
            self.nested_function_depth -= 1;
        }
        fn visit_super(&mut self, _it: &oxc_ast::ast::Super) {
            if self.nested_function_depth == 0 && !self.found {
                self.found = true;
            }
        }
    }

    for prop in &obj.properties {
        let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) = prop else {
            continue;
        };
        let is_function_like = p.method
            || matches!(
                p.kind,
                oxc_ast::ast::PropertyKind::Get | oxc_ast::ast::PropertyKind::Set
            );
        if !is_function_like {
            continue;
        }
        let oxc_ast::ast::Expression::FunctionExpression(func) = &p.value else {
            continue;
        };
        let mut finder = SuperFinder {
            found: false,
            nested_function_depth: 0,
        };
        if let Some(body) = func.body.as_deref() {
            for stmt in &body.statements {
                finder.visit_statement(stmt);
            }
        }
        for param in &func.params.items {
            if let Some(init) = param.initializer.as_deref() {
                finder.visit_expression(init);
            }
        }
        if finder.found {
            return true;
        }
    }
    false
}
