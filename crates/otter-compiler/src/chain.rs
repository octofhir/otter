//! Optional chaining lowering and chain-object helpers.
//!
//! # Contents
//! - chain expression dispatch
//! - chain object and callee evaluation
//! - chain element conversion
//! - chain span helpers
//!
//! # Invariants
//! - Nullish short-circuit patching is owned by the chain lowering path.
//!
//! # See also
//! - `calls` and `expr`

use crate::*;

/// Lower an optional chain `a?.b?.c?.()` per §13.3.9.
///
/// # Algorithm
/// 1. Walk to the chain root, collecting each step (member access /
///    call) and its `optional` flag in source order.
/// 2. Compile the root, then apply each step:
///    - Evaluate the receiver into a scratch register.
///    - If the step is optional, emit `JumpIfNullish receiver →
///      exit` to short-circuit when the receiver is `null` or
///      `undefined`. The exit target writes `undefined` into the
///      result register.
///    - Otherwise, perform the property load / call as usual.
/// 3. After the final step writes its value, emit an unconditional
///    jump past the exit handler so the chain's success result lands
///    directly in the output register.
///
/// Foundation simplifications:
/// - Optional `super` chains (`super?.foo`) are illegal per §15.6.4
///   and not exercised here.
/// - `delete a?.b` follows the no-op rule §13.3.9.5; foundation
///   relies on the chain producing `undefined` and the regular
///   `delete` path handling the rest.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-optional-chains>
/// - <https://tc39.es/ecma262/#sec-optional-chaining-evaluation>
pub(crate) fn compile_chain_expression(
    cx: &mut Compiler,
    chain: &oxc_ast::ast::ChainExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (chain.span.start, chain.span.end);
    let result = cx.alloc_scratch();
    let exits = compile_chain_into(cx, &chain.expression, result)?;
    // Success path falls through here — jump past the undefined
    // writer so the chain's value lives in `result`.
    let join = cx.emit_branch_placeholder(Op::Jump, None, span);
    // Land every optional-step short-circuit at the undefined writer.
    for pc in exits {
        cx.patch_branch_to_here(pc);
    }
    cx.emit(Op::LoadUndefined, [Operand::Register(result)], span);
    cx.patch_branch_to_here(join);
    Ok(result)
}

/// Recursive helper: compile one chain element, writing the success
/// result into `result_reg`. Returns the list of short-circuit
/// jump PCs that should land at the chain's `undefined` writer.
pub(crate) fn compile_chain_into(
    cx: &mut Compiler,
    elem: &oxc_ast::ast::ChainElement<'_>,
    result_reg: u16,
) -> Result<Vec<u32>, CompileError> {
    use oxc_ast::ast::ChainElement;
    match elem {
        ChainElement::CallExpression(call) => {
            let span = (call.span.start, call.span.end);
            let mut exits: Vec<u32> = Vec::new();
            // §13.3.9.1 — a member-expression callee passes its base
            // object as `this` (`a?.b()` calls with this = a); bare
            // callees call with undefined.
            let (callee_reg, this_reg): (u16, Option<u16>) = match &call.callee {
                expr if matches!(
                    expression_as_chain_element(expr),
                    Some(ChainObjectRef::Static(_) | ChainObjectRef::Computed(_))
                ) =>
                {
                    match expression_as_chain_element(expr) {
                        Some(ChainObjectRef::Static(m)) => {
                            let mspan = (m.span.start, m.span.end);
                            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
                            if m.optional {
                                let pc = cx.emit_branch_placeholder(
                                    Op::JumpIfNullish,
                                    Some(obj_reg),
                                    mspan,
                                );
                                exits.push(pc);
                            }
                            let callee = cx.alloc_scratch();
                            let name_idx = cx.intern_string_constant(m.property.name.as_str());
                            cx.emit(
                                Op::LoadProperty,
                                vec![
                                    Operand::Register(callee),
                                    Operand::Register(obj_reg),
                                    Operand::ConstIndex(name_idx),
                                ],
                                mspan,
                            );
                            (callee, Some(obj_reg))
                        }
                        Some(ChainObjectRef::Computed(m)) => {
                            let mspan = (m.span.start, m.span.end);
                            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
                            if m.optional {
                                let pc = cx.emit_branch_placeholder(
                                    Op::JumpIfNullish,
                                    Some(obj_reg),
                                    mspan,
                                );
                                exits.push(pc);
                            }
                            let key_reg = compile_expr(cx, &m.expression, mspan)?;
                            let callee = cx.alloc_scratch();
                            cx.emit(
                                Op::LoadElement,
                                vec![
                                    Operand::Register(callee),
                                    Operand::Register(obj_reg),
                                    Operand::Register(key_reg),
                                ],
                                mspan,
                            );
                            (callee, Some(obj_reg))
                        }
                        _ => unreachable!("guarded by matches!"),
                    }
                }
                expr => (compile_chain_callee(cx, expr, &mut exits)?, None),
            };
            if call.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(callee_reg), span);
                exits.push(pc);
            }
            // Compile call arguments.
            let mut arg_regs: Vec<u16> = Vec::with_capacity(call.arguments.len());
            for arg in call.arguments.iter() {
                if let Some(expr) = arg.as_expression() {
                    arg_regs.push(compile_expr(cx, expr, span)?);
                } else {
                    return Err(CompileError::Unsupported {
                        node: "ChainExpression: spread argument".to_string(),
                        span,
                    });
                }
            }
            crate::calls::check_call_arity(arg_regs.len(), "Op::Call", span)?;
            match this_reg {
                Some(this_reg) => {
                    let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
                    operands.push(Operand::Register(result_reg));
                    operands.push(Operand::Register(callee_reg));
                    operands.push(Operand::Register(this_reg));
                    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                    operands.extend(arg_regs.into_iter().map(Operand::Register));
                    cx.emit(Op::CallWithThis, operands, span);
                }
                None => {
                    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
                    operands.push(Operand::Register(result_reg));
                    operands.push(Operand::Register(callee_reg));
                    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                    operands.extend(arg_regs.into_iter().map(Operand::Register));
                    cx.emit(Op::Call, operands, span);
                }
            }
            Ok(exits)
        }
        ChainElement::StaticMemberExpression(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            Ok(exits)
        }
        ChainElement::ComputedMemberExpression(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            let key_reg = compile_expr(cx, &m.expression, span)?;
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            Ok(exits)
        }
        ChainElement::PrivateFieldExpression(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            crate::class::emit_private_method_brand_check(
                cx,
                obj_reg,
                m.field.name.as_str(),
                span,
            )?;
            let key_reg = crate::class::load_private_key(cx, m.field.name.as_str(), span)?;
            cx.emit(
                Op::PrivateGet,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            Ok(exits)
        }
        other => Err(CompileError::Unsupported {
            node: format!("ChainElement ({:?})", std::mem::discriminant(other)),
            span: (0, 0),
        }),
    }
}

/// Compile a chain object — either another chain step (recurse) or a
/// regular expression. Threads short-circuit jump PCs upward.
pub(crate) fn compile_chain_object(
    cx: &mut Compiler,
    expr: &oxc_ast::ast::Expression<'_>,
    exits: &mut Vec<u32>,
) -> Result<u16, CompileError> {
    if let Some(elem) = expression_as_chain_element(expr) {
        let result_reg = cx.alloc_scratch();
        let inner = compile_chain_into_chain_object(cx, elem, result_reg)?;
        exits.extend(inner);
        return Ok(result_reg);
    }
    let span = expression_span(expr);
    compile_expr(cx, expr, span)
}

/// Same as [`compile_chain_object`] but accepts a callee position
/// (the callee of `a?.b()`'s call step).
pub(crate) fn compile_chain_callee(
    cx: &mut Compiler,
    expr: &oxc_ast::ast::Expression<'_>,
    exits: &mut Vec<u32>,
) -> Result<u16, CompileError> {
    if let Some(elem) = expression_as_chain_element(expr) {
        let result_reg = cx.alloc_scratch();
        let inner = compile_chain_into_chain_object(cx, elem, result_reg)?;
        exits.extend(inner);
        return Ok(result_reg);
    }
    let span = expression_span(expr);
    compile_expr(cx, expr, span)
}

/// Internal — same as [`compile_chain_into`] but borrows the element
/// reference rather than cloning, since OXC doesn't expose a free
/// conversion. We inline the dispatch here.
pub(crate) fn compile_chain_into_chain_object(
    cx: &mut Compiler,
    elem: ChainObjectRef<'_>,
    result_reg: u16,
) -> Result<Vec<u32>, CompileError> {
    match elem {
        ChainObjectRef::Static(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            Ok(exits)
        }
        ChainObjectRef::Private(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            crate::class::emit_private_method_brand_check(
                cx,
                obj_reg,
                m.field.name.as_str(),
                span,
            )?;
            let key_reg = crate::class::load_private_key(cx, m.field.name.as_str(), span)?;
            cx.emit(
                Op::PrivateGet,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            Ok(exits)
        }
        ChainObjectRef::Computed(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            let key_reg = compile_expr(cx, &m.expression, span)?;
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            Ok(exits)
        }
        ChainObjectRef::Call(c) => {
            let span = (c.span.start, c.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let callee_reg = compile_chain_callee(cx, &c.callee, &mut exits)?;
            if c.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(callee_reg), span);
                exits.push(pc);
            }
            let mut arg_regs: Vec<u16> = Vec::with_capacity(c.arguments.len());
            for arg in c.arguments.iter() {
                if let Some(expr) = arg.as_expression() {
                    arg_regs.push(compile_expr(cx, expr, span)?);
                } else {
                    return Err(CompileError::Unsupported {
                        node: "ChainExpression: spread argument".to_string(),
                        span,
                    });
                }
            }
            crate::calls::check_call_arity(arg_regs.len(), "Op::Call", span)?;
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(result_reg));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::Call, operands, span);
            Ok(exits)
        }
    }
}

/// Borrowed handle for an inner chain step — avoids cloning OXC's
/// arena-allocated nodes.
pub(crate) enum ChainObjectRef<'a> {
    Static(&'a oxc_ast::ast::StaticMemberExpression<'a>),
    Computed(&'a oxc_ast::ast::ComputedMemberExpression<'a>),
    Call(&'a oxc_ast::ast::CallExpression<'a>),
    Private(&'a oxc_ast::ast::PrivateFieldExpression<'a>),
}

pub(crate) fn expression_as_chain_element<'a>(
    expr: &'a oxc_ast::ast::Expression<'a>,
) -> Option<ChainObjectRef<'a>> {
    match expr {
        Expression::StaticMemberExpression(m) => Some(ChainObjectRef::Static(m)),
        Expression::ComputedMemberExpression(m) => Some(ChainObjectRef::Computed(m)),
        Expression::CallExpression(c) => Some(ChainObjectRef::Call(c)),
        Expression::PrivateFieldExpression(m) => Some(ChainObjectRef::Private(m)),
        _ => None,
    }
}

pub(crate) fn expression_span(expr: &oxc_ast::ast::Expression<'_>) -> (u32, u32) {
    use oxc_span::GetSpan;
    let s = expr.span();
    (s.start, s.end)
}
