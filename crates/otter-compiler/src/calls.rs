//! Call-expression and spread-call lowering helpers.
//!
//! # Contents
//! - method calls
//! - spread calls
//! - Function method fast paths
//! - argument list compilation
//! - array spreading
//!
//! # Invariants
//! - Callee lookup precedes observable argument evaluation. Fused calls require
//!   a proof that argument reads and nonthrowing operations commute with it.
//! - Argument registers are emitted in source order.
//! - Mutable builtin names use the same resolution as ordinary callables.
//!
//! # See also
//! - `chain` and environment-aware identifier lowering

use crate::*;

#[cfg(test)]
#[path = "calls_tests.rs"]
mod tests;

/// Maximum argument count for dense `Op::Call` / `Op::CallWithThis`
/// / `Op::CallMethodValue` / `Op::BindFunction` lowering. The
/// bytecode wire encoder packs the per-instruction operand count
/// into one `u8`, so the variadic tail-payload of arguments tops out
/// at `u8::MAX` minus the fixed-position leading operands (`dst`,
/// optional callee/receiver, count-imm). 240 leaves enough headroom
/// across every variadic call op and matches the
/// `DENSE_NEW_ARRAY_MAX_ELEMENTS` threshold so the encoder layout is
/// uniform across families.
pub(crate) const MAX_DENSE_CALL_ARGS: usize = 240;

/// Reject argument lists that exceed [`MAX_DENSE_CALL_ARGS`] with a
/// catchable `CompileError` instead of panicking inside the wire
/// encoder. Generated code that fans out to thousands of positional
/// arguments should use spread (`f(...args)`), which goes through
/// `Op::CallSpread` and is unaffected by the cap.
pub(crate) fn check_call_arity(
    arg_count: usize,
    op_name: &'static str,
    span: (u32, u32),
) -> Result<(), CompileError> {
    if arg_count > MAX_DENSE_CALL_ARGS {
        return Err(CompileError::Unsupported {
            node: format!(
                "{op_name} with {arg_count} arguments exceeds the {MAX_DENSE_CALL_ARGS}-arg limit; \
                 use spread (`f(...args)`) for very wide call sites",
            ),
            span,
        });
    }
    Ok(())
}

/// Lower calls with their resolved callable, receiver and evaluated arguments.
///
/// A static method uses the fused [`Op::CallMethodValue`] only when its
/// arguments commute with lookup. Other members load the callable first and
/// use [`Op::CallWithThis`]. Free calls use [`Op::Call`]. A proved forwarding
/// shape retains the live `apply` lookup and the existing argument-frame path.
///
/// §13.3.6.1 — parenthesizing the callee of a call does NOT make an
/// `eval` call indirect: `(eval)(x)` and `((eval))(x)` are direct eval,
/// only `(1, eval)(x)` (a non-trivial CoverParenthesizedExpression) is
/// indirect. oxc keeps `ParenthesizedExpression` nodes, so unwrap them
/// when — and only when — they wrap the bare `eval` identifier, leaving
/// every other parenthesized callee (e.g. `(obj.m)()`, `(1, eval)()`)
/// untouched so their normal call semantics are preserved.
fn peel_paren_eval<'a>(callee: &'a Expression<'a>) -> &'a Expression<'a> {
    if let Expression::ParenthesizedExpression(p) = callee {
        let inner = peel_paren_eval(&p.expression);
        if matches!(inner, Expression::Identifier(id) if id.name.as_str() == "eval") {
            return inner;
        }
    }
    callee
}

pub(crate) fn compile_method_call(
    cx: &mut Compiler,
    call: &oxc_ast::ast::CallExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (call.span.start, call.span.end);
    // Callee + argument temps are dead once the call opcode has read
    // them into the callee frame (the destination is written only on
    // return), so straight-line call arms recycle the whole range into
    // `dst`. See `FunctionContext::reset_scratch`. Branch-carrying arms
    // (`with`-probe, guarded `eval`) keep their temps and are skipped.
    let call_mark = cx.scratch;
    let callee = peel_paren_eval(unwrap_ts_expr(&call.callee));
    crate::expr::mark_eager_if_function(cx, callee);
    // `super(args...)` — direct super-constructor call. Only valid
    // inside a derived-class constructor; resolving the class-scope
    // parent slot surfaces a clear diagnostic when used elsewhere.
    if let Expression::Super(_) = callee {
        return compile_super_call(cx, &call.arguments, span);
    }
    // `super.foo(args...)` — invoke a parent prototype method with
    // `this` bound to the current receiver.
    if let Expression::StaticMemberExpression(member) = callee
        && matches!(member.object, Expression::Super(_))
    {
        return compile_super_method_call(cx, member.property.name.as_str(), &call.arguments, span);
    }
    // §13.3.9.1 — `(a?.b)(...)`: a parenthesized chain callee keeps
    // its Reference base as `this`.
    if let Some((callee_reg, this_reg)) =
        crate::chain::try_compile_parenthesized_chain_callee(cx, callee, span)?
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        check_call_arity(arg_regs.len(), "Op::CallWithThis", span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::Register(this_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
        return Ok(dst);
    }
    // `super[expr](args...)` — computed-key parent-method invocation.
    if let Expression::ComputedMemberExpression(member) = callee
        && matches!(member.object, Expression::Super(_))
    {
        return compile_super_computed_method_call(cx, &member.expression, &call.arguments, span);
    }
    // `import.meta.resolve(specifier)` — sync URL join against the
    // active module's URL. HTML spec returns a string; foundation
    // matches that shape via `Op::ImportMetaResolve`.
    // <https://html.spec.whatwg.org/multipage/webappapis.html#hostmetagetimportmetaproperties>
    if let Expression::StaticMemberExpression(member) = callee
        && let Expression::MetaProperty(meta) = &member.object
        && meta.meta.name.as_str() == "import"
        && meta.property.name.as_str() == "meta"
        && member.property.name.as_str() == "resolve"
    {
        if call.arguments.len() != 1 {
            return Err(CompileError::Unsupported {
                node: format!("import.meta.resolve/{}", call.arguments.len()),
                span,
            });
        }
        let arg = call.arguments[0]
            .as_expression()
            .ok_or(CompileError::Unsupported {
                node: "import.meta.resolve: spread argument".to_string(),
                span,
            })?;
        let spec_reg = compile_expr(cx, arg, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::ImportMetaResolve,
            [Operand::Register(dst), Operand::Register(spec_reg)],
            span,
        );
        return Ok(dst);
    }
    let has_spread = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    // §13.3.6.1 — a direct `eval` call stays direct with spread
    // arguments (ArgumentListEvaluation drains the iterables, the
    // first element is the eval text); the eval interception below
    // owns that shape.
    // Direct eval only when `eval` is the global intrinsic — i.e. not
    // shadowed by a binding anywhere in the lexical chain (a captured
    // outer `var eval` counts), and not a module import.
    let is_bare_eval = matches!(callee, Expression::Identifier(id) if id.name.as_str() == "eval")
        && cx.binding_position("eval").is_none()
        && find_module_import_binding(cx, "eval").is_none();
    if has_spread && !is_bare_eval {
        return compile_spread_call(cx, callee, &call.arguments, span);
    }
    // §9.1.1.2 — a call through a `with` binding carries the binding
    // object's receiver. Direct eval retains its independent identity probe.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() != "eval"
        && !cx.active_with_envs.is_empty()
        && cx.lookup_binding(id.name.as_str()).is_none()
    {
        // §13.3.6.2 EvaluateCall step 1.b.ii — when the reference
        // resolves through an object environment record, `this` is
        // the record's WithBaseObject (the `with` object), not
        // undefined. Inline the probe so the matched branch can
        // carry both the callee value and that base.
        let name = id.name.as_str().to_string();
        let active_with_envs = cx.active_with_envs.clone();
        let callee_reg = cx.alloc_scratch();
        let this_reg = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, [Operand::Register(this_reg)], span);
        let probe =
            crate::with_statement::emit_with_binding_probe(cx, &name, &active_with_envs, span)?;
        let mut with_done = None;
        if let Some(probe) = &probe {
            let fallback = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
            crate::with_statement::emit_with_get_binding_value(
                cx,
                callee_reg,
                probe.object_reg,
                &name,
                span,
            );
            cx.emit(
                Op::StoreLocal,
                [
                    Operand::Register(probe.object_reg),
                    Operand::Imm32(this_reg as i32),
                ],
                span,
            );
            with_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
            cx.patch_branch_to_here(fallback);
        }
        let fallback_value =
            crate::expr::identifier::compile_identifier_without_with(cx, &name, span)?;
        cx.emit(
            Op::StoreLocal,
            [
                Operand::Register(fallback_value),
                Operand::Imm32(callee_reg as i32),
            ],
            span,
        );
        if let Some(done) = with_done {
            cx.patch_branch_to_here(done);
        }
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        check_call_arity(arg_regs.len(), "Op::CallWithThis", span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::Register(this_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
        return Ok(dst);
    }
    // §19.4.1 `eval(source)` — bare-identifier interception.
    // Foundation ships indirect-eval semantics (fresh global
    // scope) which keeps the implementation tractable while
    // covering the common use case of running source-string
    // payloads at runtime.
    // <https://tc39.es/ecma262/#sec-eval-x>
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "eval"
    {
        // §13.3.6.1 — a bare `eval(...)` call is a direct eval whenever the
        // resolved value is %eval%, even through a local binding or `with`
        // object; the guarded lowering decides at runtime. The spread form
        // keeps the static unshadowed requirement.
        let statically_unshadowed = cx.binding_position("eval").is_none()
            && find_module_import_binding(cx, "eval").is_none();
        if has_spread && statically_unshadowed {
            // Spread form: drain every argument into an array per
            // ArgumentListEvaluation, then take element 0 as the eval
            // text. The rare spread `eval(...)` keeps the unconditional
            // `Op::Eval` lowering; the runtime eval-identity guard is
            // applied only to the common non-spread fast path below.
            let arr = cx.alloc_scratch();
            cx.emit(
                Op::NewArray,
                [Operand::Register(arr), Operand::ConstIndex(0)],
                span,
            );
            for arg in &call.arguments {
                match arg {
                    oxc_ast::ast::Argument::SpreadElement(s) => {
                        let inner_span = (s.span.start, s.span.end);
                        emit_spread_into_array(cx, arr, &s.argument, inner_span)?;
                    }
                    other => {
                        let r = compile_expr(cx, other.to_expression(), span)?;
                        cx.emit(
                            Op::ArrayPush,
                            [Operand::Register(arr), Operand::Register(r)],
                            span,
                        );
                    }
                }
            }
            let idx = cx.alloc_scratch();
            cx.emit(
                Op::LoadInt32,
                [Operand::Register(idx), Operand::Imm32(0)],
                span,
            );
            let src_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadElement,
                [
                    Operand::Register(src_reg),
                    Operand::Register(arr),
                    Operand::Register(idx),
                ],
                span,
            );
            let dst = cx.alloc_scratch();
            let flags = compute_eval_flags(cx);
            emit_direct_eval(cx, dst, src_reg, flags, span);
            return Ok(dst);
        }
        if !has_spread {
            return emit_guarded_eval(cx, call, callee, span, false);
        }
    }
    if let Expression::StaticMemberExpression(member) = callee {
        let method_name = member.property.name.as_str();
        if let Some(dst) = try_compile_forwarded_apply(cx, call, member, span)? {
            return Ok(dst);
        }
        // The call reads the receiver after every argument: a binding held in
        // its own frame slot is passed in place unless an argument assigns it.
        let later: Vec<&Expression<'_>> = call
            .arguments
            .iter()
            .filter_map(|argument| argument.as_expression())
            .collect();
        let receiver_reg = match crate::expr::borrowed_operand_before(cx, &member.object, &later) {
            Some(reg)
                if call
                    .arguments
                    .iter()
                    .all(|argument| argument.as_expression().is_some()) =>
            {
                reg
            }
            _ => compile_expr(cx, &member.object, span)?,
        };
        let name_idx = cx.intern_string_constant(method_name);
        if !call_arguments_commute_with_method_lookup(cx, &call.arguments) {
            // §13.3.6.1 — GetValue on the callee reference precedes
            // ArgumentListEvaluation, so the method read is its own
            // instruction here: a getter runs, and a nullish base throws,
            // before any argument evaluates.
            let callee_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                [
                    Operand::Register(callee_reg),
                    Operand::Register(receiver_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            check_call_arity(arg_regs.len(), "Op::CallWithThis", span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::Register(receiver_reg));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::CallWithThis, operands, span);
            return Ok(dst);
        }
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        check_call_arity(arg_regs.len(), "Op::CallMethodValue", span)?;
        cx.reset_scratch(call_mark);
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(name_idx));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallMethodValue, operands, span);
        return Ok(dst);
    }
    if let Expression::PrivateFieldExpression(member) = callee {
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        crate::class::emit_private_method_brand_check(
            cx,
            receiver_reg,
            member.field.name.as_str(),
            span,
        )?;
        let key_reg = crate::class::load_private_key(cx, member.field.name.as_str(), span)?;
        let callee_reg = cx.alloc_scratch();
        cx.emit(
            Op::PrivateGet,
            [
                Operand::Register(callee_reg),
                Operand::Register(receiver_reg),
                Operand::Register(key_reg),
            ],
            span,
        );
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        check_call_arity(arg_regs.len(), "Op::CallMethodValue", span)?;
        cx.reset_scratch(call_mark);
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
        return Ok(dst);
    }
    // `obj[expr](args...)` — computed-member call. Lower as
    // `LoadElement` + `CallWithThis` so the callee receives the
    // receiver as its `this` value, matching ECMA-262 §13.3.6.1
    // EvaluateCall step 5.b.
    if let Expression::ComputedMemberExpression(member) = callee {
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        if let Expression::StringLiteral(lit) = unwrap_ts_expr(&member.expression)
            && !lit.lone_surrogates
            && call_arguments_commute_with_method_lookup(cx, &call.arguments)
        {
            let name_idx = cx.intern_string_constant(lit.value.as_str());
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            check_call_arity(arg_regs.len(), "Op::CallMethodValue", span)?;
            cx.reset_scratch(call_mark);
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(receiver_reg));
            operands.push(Operand::ConstIndex(name_idx));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::CallMethodValue, operands, span);
            return Ok(dst);
        }
        let idx_reg = compile_expr(cx, &member.expression, span)?;
        let callee_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadElement,
            [
                Operand::Register(callee_reg),
                Operand::Register(receiver_reg),
                Operand::Register(idx_reg),
            ],
            span,
        );
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        check_call_arity(arg_regs.len(), "Op::CallWithThis", span)?;
        cx.reset_scratch(call_mark);
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
        return Ok(dst);
    }
    // Free call: `callee(args...)`.
    let callee_reg = compile_expr(cx, callee, span)?;
    let arg_regs = compile_call_args(cx, &call.arguments, span)?;
    check_call_arity(arg_regs.len(), "Op::Call", span)?;
    cx.reset_scratch(call_mark);
    let dst = cx.alloc_scratch();
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(callee_reg));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::Call, operands, span);
    Ok(dst)
}

/// §15.10.3 — when a `CallExpression` sits in a strict-mode tail
/// position, return the (peeled) callee if it is a *free* call the
/// plain [`Op::TailCall`] path can lower. Member / `super` / optional /
/// spread / direct-`eval` calls return `None`: they either need a
/// receiver (a different opcode) or have non-tail semantics, so the
/// caller falls back to an ordinary call + return.
///
/// A `with` environment could otherwise redirect a free callee name,
/// but tail-call lowering only runs in strict mode where `with` is a
/// SyntaxError, so the free-identifier fast paths stay sound.
pub(crate) fn tail_eligible_free_callee<'a>(
    cx: &Compiler,
    call: &'a oxc_ast::ast::CallExpression<'a>,
) -> Option<&'a Expression<'a>> {
    if call.optional {
        return None;
    }
    let has_spread = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    if has_spread {
        return None;
    }
    let callee = peel_paren_eval(unwrap_ts_expr(&call.callee));
    match callee {
        Expression::Super(_)
        | Expression::StaticMemberExpression(_)
        | Expression::ComputedMemberExpression(_)
        | Expression::PrivateFieldExpression(_)
        | Expression::MetaProperty(_) => None,
        // A direct `eval(...)` keeps its bespoke `Op::Eval` lowering
        // (lexical-scope capture); only a *shadowed* `eval` local is a
        // plain call eligible for tail-call lowering.
        Expression::Identifier(id)
            if id.name.as_str() == "eval"
                && cx.binding_position("eval").is_none()
                && find_module_import_binding(cx, "eval").is_none() =>
        {
            None
        }
        _ => Some(callee),
    }
}

/// Emit an `Op::TailCall` for a free call leaf, returning the dst
/// register the fallback `Op::ReturnValue` reads. Mirrors the free-call
/// arm of [`compile_method_call`] but with the tail opcode.
pub(crate) fn emit_tail_free_call(
    cx: &mut Compiler,
    call: &oxc_ast::ast::CallExpression<'_>,
    callee: &Expression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let callee_reg = compile_expr(cx, callee, span)?;
    let arg_regs = compile_call_args(cx, &call.arguments, span)?;
    check_call_arity(arg_regs.len(), "Op::TailCall", span)?;
    let dst = cx.alloc_scratch();
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(callee_reg));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::TailCall, operands, span);
    Ok(dst)
}

/// Emit `Op::Eval` over the innermost context at the call site: the eval
/// body resolves names through that context chain.
fn emit_direct_eval(cx: &mut Compiler, dst: u16, src_reg: u16, flags: i32, span: (u32, u32)) {
    let ctx = cx.innermost_ctx();
    cx.emit_ctx(
        Op::Eval,
        [
            Operand::Register(dst),
            Operand::Register(src_reg),
            Operand::Register(0),
            Operand::Imm32(flags),
        ],
        2,
        ctx,
        span,
    );
}

/// §19.2.1.1 eval-body flag bits shared by the direct-eval lowerings.
/// Bit 0 forbid var-`arguments`; 1 parameter-initializer window; 2
/// `new.target` legal; 3 field-initializer (new.target undefined); 4
/// `super.x` legal (HomeObject in scope); 5 `super()` legal.
pub(crate) fn compute_eval_flags(cx: &mut Compiler) -> i32 {
    let forbid_var_arguments = cx.in_param_init && cx.binds_arguments;
    let new_target_allowed = cx.eval_new_target_allowed
        || cx.in_field_initializer
        || cx.stack.iter().skip(1).any(|frame| !frame.is_arrow);
    // §19.2.1.1 — `super.x` in eval is legal only when the innermost
    // non-arrow enclosing function carries a [[HomeObject]] (method,
    // class constructor, static block). A plain function nested in a
    // method is opaque even though its context chain could still reach
    // the home slot.
    let home_frame = cx
        .stack
        .iter()
        .rev()
        .find(|frame| !frame.is_arrow)
        .is_some_and(|frame| frame.has_home_object);
    let super_allowed = cx.in_field_initializer
        || (home_frame
            && (cx.resolve_name(crate::class::SUPER_HOME_NAME).is_some()
                || cx
                    .resolve_name(crate::class::SUPER_STATIC_HOME_NAME)
                    .is_some()));
    // §19.2.1.1 — `super()` in eval is legal only when the innermost
    // non-arrow enclosing function is a derived class constructor —
    // and never in a field initializer (§15.7.1 ClassElementName
    // Contains SuperCall is a Syntax Error even though initializers
    // compile into the constructor frame).
    let super_call_allowed = !cx.in_field_initializer
        && cx
            .stack
            .iter()
            .rev()
            .find(|frame| !frame.is_arrow)
            .is_some_and(|frame| frame.is_derived_ctor)
        && (cx.resolve_name(crate::class::SUPER_CTOR_NAME).is_some()
            || cx.resolve_name(crate::class::CLASS_SELF_NAME).is_some());
    i32::from(forbid_var_arguments)
        | (i32::from(cx.in_param_init) << 1)
        | (i32::from(new_target_allowed) << 2)
        | (i32::from(cx.in_field_initializer) << 3)
        | (i32::from(super_allowed) << 4)
        | (i32::from(super_call_allowed) << 5)
}

/// `true` for a syntactic non-spread `eval(...)` site whose callee name
/// is unshadowed (so the static lowering would treat it as direct eval).
/// The runtime guard in [`emit_guarded_eval`] still decides direct vs.
/// ordinary based on the callee's actual value.
pub(crate) fn is_syntactic_eval_call(
    cx: &Compiler,
    call: &oxc_ast::ast::CallExpression<'_>,
) -> bool {
    if call.optional {
        return false;
    }
    let has_spread = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    if has_spread {
        return false;
    }
    matches!(peel_paren_eval(unwrap_ts_expr(&call.callee)), Expression::Identifier(id)
        if id.name.as_str() == "eval"
            && cx.binding_position("eval").is_none()
            && find_module_import_binding(cx, "eval").is_none())
}

/// §sec-function-calls step 6.a — lower a syntactic non-spread
/// `eval(args)` with a runtime `SameValue(func, %eval%)` guard: evaluate
/// the `eval` reference and the arguments once, then branch on
/// [`Op::IsEvalIntrinsic`]. When the resolved value is the real `eval`,
/// run `Op::Eval` (direct eval with caller scope); otherwise perform an
/// ordinary call — a tail call when `tail` is set — to the shadowing
/// value. Both branches land their result in the same `dst`.
pub(crate) fn emit_guarded_eval(
    cx: &mut Compiler,
    call: &oxc_ast::ast::CallExpression<'_>,
    callee: &Expression<'_>,
    span: (u32, u32),
    tail: bool,
) -> Result<u16, CompileError> {
    // Evaluate the callee reference before the arguments (spec order),
    // resolving `eval` through whatever scope applies (global, captured
    // outer binding, `with` object, or a dynamically-introduced var).
    let ev_reg = compile_expr(cx, callee, span)?;
    let arg_regs = compile_call_args(cx, &call.arguments, span)?;
    check_call_arity(arg_regs.len(), "Op::TailCall", span)?;
    let dst = cx.alloc_scratch();
    let src_reg = match arg_regs.first() {
        Some(&r) => r,
        None => {
            let u = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(u)], span);
            u
        }
    };
    let flags = compute_eval_flags(cx);

    let is_eval = cx.alloc_scratch();
    cx.emit(
        Op::IsEvalIntrinsic,
        [Operand::Register(is_eval), Operand::Register(ev_reg)],
        span,
    );
    let to_ordinary = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(is_eval), span);
    // Direct-eval branch.
    emit_direct_eval(cx, dst, src_reg, flags, span);
    let to_end = cx.emit_branch_placeholder(Op::Jump, None, span);
    // Ordinary-call branch — `eval` was shadowed at runtime.
    cx.patch_branch_to_here(to_ordinary);
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(ev_reg));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.iter().map(|&r| Operand::Register(r)));
    cx.emit(if tail { Op::TailCall } else { Op::Call }, operands, span);
    cx.patch_branch_to_here(to_end);
    Ok(dst)
}

/// Lower a call expression whose argument list contains at least
/// one `...spread` element to [`Op::CallSpread`]. Two callee
/// shapes are handled:
///
/// - `obj.method(...args)` — receiver is evaluated once, the spread
///   args become an array, dispatched with `this = obj`.
/// - `callee(...args)` — free call, dispatched with
///   `this = undefined`.
///
/// Mixed spread / non-spread arguments are folded into the same
/// args array so `f(a, ...arr, b)` calls `f(a, ...arr items..., b)`.
pub(crate) fn compile_spread_call(
    cx: &mut Compiler,
    callee: &Expression<'_>,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let (callee_reg, this_reg) = match callee {
        Expression::StaticMemberExpression(member) => {
            let recv = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let method_dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                [
                    Operand::Register(method_dst),
                    Operand::Register(recv),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            (method_dst, recv)
        }
        // `obj[expr](...args)` — computed-member spread call. The
        // receiver must be threaded as `this`, mirroring the
        // non-spread computed-member path (§13.3.6.1 EvaluateCall
        // step 5.b). Object then property key then args, in source
        // order.
        Expression::ComputedMemberExpression(member) => {
            let recv = compile_expr(cx, &member.object, span)?;
            let idx_reg = compile_expr(cx, &member.expression, span)?;
            let method_dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadElement,
                [
                    Operand::Register(method_dst),
                    Operand::Register(recv),
                    Operand::Register(idx_reg),
                ],
                span,
            );
            (method_dst, recv)
        }
        other => {
            let r = compile_expr(cx, other, span)?;
            let this_dst = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(this_dst)], span);
            (r, this_dst)
        }
    };
    let args_reg = compile_spread_call_args(cx, arguments, span)?;
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::CallSpread,
        [
            Operand::Register(dst),
            Operand::Register(callee_reg),
            Operand::Register(this_reg),
            Operand::Register(args_reg),
        ],
        span,
    );
    Ok(dst)
}

/// Lower `<callee>.apply(<this>, arguments)` to [`Op::CallForwardArguments`]
/// in a body whose every `arguments` reference is such a forward.
///
/// The method read stays an observable `LoadProperty` before the `this`
/// argument evaluates (§13.3.6.1); the call itself forwards the
/// activation's actual arguments when that read produced the intrinsic and
/// otherwise calls the resolved method with a lazily built arguments object.
fn try_compile_forwarded_apply(
    cx: &mut Compiler,
    call: &oxc_ast::ast::CallExpression<'_>,
    member: &oxc_ast::ast::StaticMemberExpression<'_>,
    span: (u32, u32),
) -> Result<Option<u16>, CompileError> {
    use oxc_ast::ast::Argument;
    if call.optional
        || member.optional
        || member.property.name.as_str() != "apply"
        || call.arguments.len() != 2
        || !cx.top_mut().arguments_forward_only
        || cx.lookup_binding("arguments").is_some()
    {
        return Ok(None);
    }
    let (Some(this_argument), Some(Argument::Identifier(forwarded))) =
        (call.arguments[0].as_expression(), Some(&call.arguments[1]))
    else {
        return Ok(None);
    };
    if forwarded.name.as_str() != "arguments" {
        return Ok(None);
    }
    let receiver_reg = compile_expr(cx, &member.object, span)?;
    let method_reg = cx.alloc_scratch();
    let name_idx = cx.intern_string_constant("apply");
    cx.emit(
        Op::LoadProperty,
        [
            Operand::Register(method_reg),
            Operand::Register(receiver_reg),
            Operand::ConstIndex(name_idx),
        ],
        span,
    );
    let this_reg = compile_expr(cx, this_argument, span)?;
    let dst = cx.alloc_scratch();
    // The mapped formals' context register; any register when none is
    // context-held.
    let mapped_ctx = cx.top_mut().mapped_arguments_ctx.unwrap_or(dst);
    cx.emit(
        Op::CallForwardArguments,
        [
            Operand::Register(dst),
            Operand::Register(method_reg),
            Operand::Register(receiver_reg),
            Operand::Register(this_reg),
            Operand::Register(mapped_ctx),
        ],
        span,
    );
    Ok(Some(dst))
}

/// Prove that these arguments commute with the callee's property lookup.
///
/// The fused opcode reads its method after filling argument registers. Every
/// argument must be nonthrowing and independent of getter writes: an immutable
/// initialized binding or a private register suffices; mutable context slots,
/// dynamic lookup and coercion of unknown values do not.
pub(crate) fn call_arguments_commute_with_method_lookup(
    cx: &Compiler,
    args: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
) -> bool {
    args.iter().all(|arg| match arg {
        oxc_ast::ast::Argument::SpreadElement(_) => false,
        other => expression_commutes_with_method_lookup(cx, other.to_expression()),
    })
}

fn expression_commutes_with_method_lookup(cx: &Compiler, expr: &Expression<'_>) -> bool {
    use oxc_ast::ast::{ObjectPropertyKind, PropertyKey, UnaryOperator};

    match unwrap_ts_expr(expr) {
        Expression::NumericLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_)
        | Expression::BigIntLiteral(_)
        | Expression::RegExpLiteral(_)
        | Expression::FunctionExpression(_)
        | Expression::ArrowFunctionExpression(_) => true,
        Expression::ThisExpression(_) => cx
            .stack
            .iter()
            .rev()
            .find(|frame| !frame.is_arrow)
            .is_none_or(|frame| !frame.is_derived_ctor && !frame.eval_this_from_chain),
        Expression::Identifier(id) => binding_commutes_with_method_lookup(cx, id.name.as_str()),
        Expression::ParenthesizedExpression(p) => {
            expression_commutes_with_method_lookup(cx, &p.expression)
        }
        Expression::UnaryExpression(unary) => {
            expression_commutes_with_method_lookup(cx, &unary.argument)
                && match unary.operator {
                    UnaryOperator::Void | UnaryOperator::LogicalNot => true,
                    UnaryOperator::UnaryPlus => {
                        numeric_primitive_kind(&unary.argument)
                            == Some(NumericPrimitiveKind::Number)
                    }
                    UnaryOperator::UnaryNegation | UnaryOperator::BitwiseNot => {
                        numeric_primitive_kind(&unary.argument).is_some()
                    }
                    _ => false,
                }
        }
        // An array literal with no spread allocates and stores; an object
        // literal additionally needs every key settled at compile time,
        // since a computed key runs ToPropertyKey.
        Expression::ArrayExpression(array) => array.elements.iter().all(|element| match element {
            oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => false,
            oxc_ast::ast::ArrayExpressionElement::Elision(_) => true,
            other => other
                .as_expression()
                .is_some_and(|expr| expression_commutes_with_method_lookup(cx, expr)),
        }),
        Expression::ObjectExpression(object) => {
            object.properties.iter().all(|property| match property {
                ObjectPropertyKind::SpreadProperty(_) => false,
                ObjectPropertyKind::ObjectProperty(entry) => {
                    !entry.computed
                        && matches!(
                            entry.key,
                            PropertyKey::StaticIdentifier(_)
                                | PropertyKey::StringLiteral(_)
                                | PropertyKey::NumericLiteral(_)
                        )
                        && expression_commutes_with_method_lookup(cx, &entry.value)
                }
            })
        }
        _ => false,
    }
}

/// A getter cannot mutate an initialized immutable binding or an unexposed
/// register. Capture, direct eval and mapped arguments move mutable bindings
/// into context slots; legacy function arguments expose a snapshot instead.
fn binding_commutes_with_method_lookup(cx: &Compiler, name: &str) -> bool {
    use crate::compiler::{Access, NameRef, ScopeLocation};

    if !cx.active_with_envs.is_empty() || find_module_import_binding(cx, name).is_some() {
        return false;
    }
    let NameRef::Binding(resolved) = cx.resolve_ref(name) else {
        return false;
    };
    let info = resolved.info;
    if resolved.lookup.is_some() || !info.initialized || info.fn_self_name {
        return false;
    }
    let may_be_uninitialized = match resolved.location {
        ScopeLocation::Frame { frame, scope } => {
            cx.stack[frame].own_access_needs_check(&info, scope)
        }
        // A descriptor names the cell but carries no current-value proof.
        ScopeLocation::Chain { .. } => info.kind.initial_hole(),
    };
    if may_be_uninitialized {
        return false;
    }
    info.is_const
        || (resolved.access == Access::Own
            && matches!(info.storage, BindingStorage::Register { .. })
            && !cx.name_needs_slot(name)
            && !cx.contains_direct_eval)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NumericPrimitiveKind {
    Number,
    BigInt,
}

/// Static value-kind proof only; advisory type annotations cannot establish
/// that numeric coercion skips user code or avoids the BigInt unary-plus throw.
fn numeric_primitive_kind(expr: &Expression<'_>) -> Option<NumericPrimitiveKind> {
    use oxc_ast::ast::UnaryOperator;

    match unwrap_ts_expr(expr) {
        Expression::NumericLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_) => Some(NumericPrimitiveKind::Number),
        Expression::BigIntLiteral(_) => Some(NumericPrimitiveKind::BigInt),
        Expression::ParenthesizedExpression(p) => numeric_primitive_kind(&p.expression),
        Expression::UnaryExpression(unary) => match unary.operator {
            UnaryOperator::Void | UnaryOperator::LogicalNot => Some(NumericPrimitiveKind::Number),
            UnaryOperator::UnaryNegation | UnaryOperator::BitwiseNot => {
                numeric_primitive_kind(&unary.argument)
            }
            UnaryOperator::UnaryPlus => numeric_primitive_kind(&unary.argument)
                .filter(|kind| *kind == NumericPrimitiveKind::Number),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn compile_call_args(
    cx: &mut Compiler,
    args: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<Vec<u16>, CompileError> {
    let mut regs: Vec<u16> = Vec::with_capacity(args.len());
    for (index, arg) in args.iter().enumerate() {
        match arg {
            oxc_ast::ast::Argument::SpreadElement(s) => {
                return Err(CompileError::Unsupported {
                    node: "Argument::SpreadElement".to_string(),
                    span: (s.span.start, s.span.end),
                });
            }
            other => {
                let expr = other.to_expression();
                // An argument naming a binding held in its own frame slot is
                // passed in place unless a later argument assigns it.
                let later: Vec<&Expression<'_>> = args[index + 1..]
                    .iter()
                    .filter_map(|later| later.as_expression())
                    .collect();
                regs.push(
                    match crate::expr::borrowed_operand_before(cx, expr, &later) {
                        Some(reg) => reg,
                        None => compile_expr(cx, expr, span)?,
                    },
                );
            }
        }
    }
    Ok(regs)
}

/// Emit the bytecode that builds a fresh `Array` register holding
/// the call arguments fanned out from spreads. Returns the
/// register that holds the resulting array. Used by the spread-in-
/// call path; pure regular argument lists keep the dedicated
/// fast path in [`compile_call_args`] / [`Op::Call`].
pub(crate) fn compile_spread_call_args(
    cx: &mut Compiler,
    args: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::NewArray,
        [Operand::Register(dst), Operand::ConstIndex(0)],
        span,
    );
    for arg in args {
        match arg {
            oxc_ast::ast::Argument::SpreadElement(s) => {
                let inner_span = (s.span.start, s.span.end);
                emit_spread_into_array(cx, dst, &s.argument, inner_span)?;
            }
            other => {
                let r = compile_expr(cx, other.to_expression(), span)?;
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

/// Append every value `iterable` iterates into the array in `dst_reg`:
/// one [`Op::SpreadAppend`]. Shared between the array-literal spread path
/// and the call-argument spread path.
pub(crate) fn emit_spread_into_array(
    cx: &mut Compiler,
    dst_reg: u16,
    iterable: &Expression<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let iterable_reg = compile_expr(cx, iterable, span)?;
    cx.emit(
        Op::SpreadAppend,
        [Operand::Register(dst_reg), Operand::Register(iterable_reg)],
        span,
    );
    Ok(())
}
