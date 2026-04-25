//! Identifier resolution (local / upvalue / global), binding
//! assertions, `yield*` delegation, optional-chain entry, and the
//! core `lower_return_expression` dispatcher that feeds every other
//! expression lowering path.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Every expression lowering in the parent
//! module ultimately re-enters through `lower_return_expression`, so
//! this module sits at the root of the dispatch tree.

use super::*;


/// Lower an `Expression::Identifier` reading the named binding into
/// the accumulator.
///
/// Resolution order:
/// 1. Local / parameter binding — routes through
///    [`lower_identifier_read`], which also primes a feedback slot
///    for M_JIT_C.2 consumption.
/// 2. Well-known global constant (M14) — emits a dedicated opcode:
///    `undefined` → `LdaUndefined`, `NaN` → `LdaNaN`, `Infinity` →
///    `LdaConstF64` against an interned `f64::INFINITY`.
/// 3. Any remaining bare identifier emits `LdaGlobal` with the
///    name interned into the function's `PropertyNameTable`.
///    This matches script-mode global environment lookup: if the
///    global binding is absent at runtime, `LdaGlobal` raises a
///    `ReferenceError` instead of rejecting valid source at
///    compile time. Test262 relies on this because harness files
///    are evaluated one after another in the same realm.
fn lower_identifier_reference(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    ident: &IdentifierReference<'_>,
) -> Result<(), SourceLoweringError> {
    let name = ident.name.as_str();
    if let Some(binding) = ctx.resolve_identifier(name) {
        return lower_identifier_read(builder, ctx, binding, ident.span);
    }
    match name {
        "undefined" => {
            builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}"))
            })?;
            Ok(())
        }
        "NaN" => {
            builder
                .emit(Opcode::LdaNaN, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaNaN: {err:?}")))?;
            Ok(())
        }
        "Infinity" => {
            let idx = ctx.intern_float_constant(f64::INFINITY)?;
            builder
                .emit(Opcode::LdaConstF64, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaConstF64: {err:?}"))
                })?;
            Ok(())
        }
        _ => {
            let idx = ctx.intern_property_name(name)?;
            builder
                .emit(Opcode::LdaGlobal, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaGlobal: {err:?}"))
                })?;
            Ok(())
        }
    }
}

fn emit_assert_not_hole(
    builder: &mut BytecodeBuilder,
    label: &'static str,
) -> Result<(), SourceLoweringError> {
    builder.emit(Opcode::AssertNotHole, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("encode AssertNotHole ({label}): {err:?}"))
    })?;
    Ok(())
}

pub(super) fn emit_load_binding_value(
    builder: &mut BytecodeBuilder,
    binding: BindingRef,
    ident_span: Span,
    label: &'static str,
) -> Result<(), SourceLoweringError> {
    match binding {
        BindingRef::Param { reg } => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar ({label}): {err:?}"))
                })?;
        }
        BindingRef::Local {
            reg,
            initialized: true,
            runtime_tdz: false,
            ..
        } => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar ({label}): {err:?}"))
                })?;
        }
        BindingRef::Local {
            reg,
            runtime_tdz: true,
            ..
        } => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar ({label}): {err:?}"))
                })?;
            emit_assert_not_hole(builder, label)?;
        }
        BindingRef::Local {
            initialized: false, ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                ident_span,
            ));
        }
        BindingRef::Upvalue { idx, .. } => {
            builder
                .emit(Opcode::LdaUpvalue, &[Operand::Idx(u32::from(idx))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaUpvalue ({label}): {err:?}"))
                })?;
            emit_assert_not_hole(builder, label)?;
        }
    }
    Ok(())
}

pub(super) fn emit_assert_binding_ready_for_write(
    builder: &mut BytecodeBuilder,
    binding: BindingRef,
    ident_span: Span,
    label: &'static str,
) -> Result<(), SourceLoweringError> {
    match binding {
        BindingRef::Local {
            reg,
            runtime_tdz: true,
            ..
        } => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar ({label}): {err:?}"))
                })?;
            emit_assert_not_hole(builder, label)
        }
        BindingRef::Param { .. }
        | BindingRef::Local {
            initialized: true, ..
        } => Ok(()),
        BindingRef::Local {
            initialized: false, ..
        } => Err(SourceLoweringError::unsupported(
            "tdz_self_reference",
            ident_span,
        )),
        BindingRef::Upvalue { is_const: true, .. } => Err(SourceLoweringError::unsupported(
            "const_assignment",
            ident_span,
        )),
        BindingRef::Upvalue {
            idx,
            is_const: false,
        } => {
            builder
                .emit(Opcode::LdaUpvalue, &[Operand::Idx(u32::from(idx))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaUpvalue ({label}): {err:?}"))
                })?;
            emit_assert_not_hole(builder, label)
        }
    }
}

/// Emits `Ldar reg` for an in-scope identifier read. Rejects
/// uninitialized locals (TDZ self-reference) at compile time so the
/// runtime never sees a hole on this path.
///
/// Allocates an arithmetic feedback slot and attaches it to the
/// emitted `Ldar` so the interpreter can record Int32 when the slot
/// holds an int32 value, and the JIT baseline can drop the `Ldar`
/// tag guard once the feedback stabilises (M_JIT_C.2 int32-trust
/// elision).
pub(super) fn lower_identifier_read(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    binding: BindingRef,
    ident_span: Span,
) -> Result<(), SourceLoweringError> {
    match binding {
        BindingRef::Param { reg }
        | BindingRef::Local {
            reg,
            initialized: true,
            runtime_tdz: false,
            ..
        } => {
            let pc = builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar (identifier read): {err:?}"))
                })?;
            let slot = ctx.allocate_arithmetic_feedback();
            builder.attach_feedback(pc, slot);
            Ok(())
        }
        other => emit_load_binding_value(builder, other, ident_span, "identifier read"),
    }
}

/// Emits a Reg-form binary opcode (`Add`/`Sub`/...) reading the given
/// in-scope identifier as the RHS. Thin wrapper over
/// [`emit_identifier_as_reg_operand`], which allocates the feedback
/// slot so the interpreter can record Int32 / NotInt32 observations
/// and the JIT baseline can consume them via
/// [`analyze_template_candidate_with_feedback`].
pub(super) fn lower_identifier_as_reg_rhs(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    encoding: &BinaryOpEncoding,
    binding: BindingRef,
    ident_span: Span,
) -> Result<(), SourceLoweringError> {
    emit_identifier_as_reg_operand(
        builder,
        ctx,
        encoding.reg_opcode,
        encoding.label,
        binding,
        ident_span,
    )?;
    Ok(())
}

/// §14.4.14 `yield* <argument>` — delegates iteration to another
/// iterable. Lowered as:
///   `GetIterator` on argument → `iter_temp`
///   loop: `IteratorStep value_temp, iter_temp`
///   if `done` (acc truthy) break
///   `Ldar value_temp; Yield`
///   jump loop_top
///   exit: `Ldar value_temp` (final value becomes expression's
///   result)
///
/// Scope: forwards values outward per spec. Sent values from
/// the outer caller's `.next(v)` reach the inner iterator only
/// as the acc at Yield resume — the inner iterator's `.next()`
/// doesn't receive them as arguments (full spec requires
/// `IteratorNext` with a sent-value operand). `.throw()` and
/// `.return()` completion forwarding are also deferred.
fn lower_yield_star<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    yield_expr: &'a oxc_ast::ast::YieldExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let Some(argument) = yield_expr.argument.as_ref() else {
        return Err(SourceLoweringError::Internal(
            "yield* without argument is a parse error".into(),
        ));
    };
    let iter_temp = ctx.acquire_temps(1)?;
    let value_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        // Resolve iterable once, stash in `value_temp` as a scratch
        // source, then convert to iterator and park in `iter_temp`.
        lower_return_expression(builder, ctx, argument)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (yield* src): {err:?}"))
            })?;
        builder
            .emit(Opcode::GetIterator, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode GetIterator (yield*): {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(iter_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (yield* iter): {err:?}"))
            })?;

        let loop_top = builder.new_label();
        let loop_exit = builder.new_label();
        builder
            .bind_label(loop_top)
            .map_err(|err| SourceLoweringError::Internal(format!("bind yield* top: {err:?}")))?;
        builder
            .emit(
                Opcode::IteratorStep,
                &[
                    Operand::Reg(u32::from(value_temp)),
                    Operand::Reg(u32::from(iter_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode IteratorStep (yield*): {err:?}"))
            })?;
        let jmp_pc = builder
            .emit_jump_to(Opcode::JumpIfToBooleanTrue, loop_exit)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfToBooleanTrue (yield* done): {err:?}"
                ))
            })?;
        ctx.attach_branch_feedback(builder, jmp_pc);
        // Forward the inner iteration's value to the outer
        // consumer via a plain Yield.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (yield* value): {err:?}"))
            })?;
        builder.emit(Opcode::Yield, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode Yield (yield*): {err:?}"))
        })?;
        builder
            .emit_jump_to(Opcode::Jump, loop_top)
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Jump (yield* back): {err:?}"))
            })?;
        builder
            .bind_label(loop_exit)
            .map_err(|err| SourceLoweringError::Internal(format!("bind yield* exit: {err:?}")))?;
        // Final value of `yield* <iter>` expression is the
        // completion value from the inner iterator's
        // `{ value: X, done: true }` — `IteratorStep` already
        // deposited `X` in `value_temp` on the terminating
        // iteration.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (yield* result): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(2);
    lower
}

/// §13.3.9 `ChainExpression` — the AST wrapper around every
/// optional-chain surface (`o?.a`, `o?.[k]`, `f?.()`, or any
/// nesting). The wrapper's `expression` is the actual
/// member/call/private-field tree that carries `optional: true`
/// on each short-circuit site.
///
/// Lowering:
///
/// ```text
///   <lower chain body, short_circuit on stack>
///   Jump end                     ; value already in acc
/// short_circuit:
///   LdaUndefined                 ; any `?.` null check lands here
/// end:
/// ```
///
/// While `short_circuit` is on the stack, the per-expression
/// lowerers (`lower_static_member_read` /
/// `lower_computed_member_read` / `lower_call_expression`) honour
/// `expr.optional` by emitting a nullish check against the
/// materialised base; otherwise those lowerers still reject
/// `optional: true` defensively.
fn lower_chain_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    chain: &'a oxc_ast::ast::ChainExpression<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::ChainElement;

    let short_circuit = builder.new_label();
    let end_label = builder.new_label();

    ctx.enter_optional_chain(short_circuit);
    let inner = match &chain.expression {
        ChainElement::CallExpression(call) => lower_call_expression(builder, ctx, call),
        ChainElement::StaticMemberExpression(member) => {
            lower_static_member_read(builder, ctx, member)
        }
        ChainElement::ComputedMemberExpression(member) => {
            lower_computed_member_read(builder, ctx, member)
        }
        ChainElement::PrivateFieldExpression(member) => {
            lower_private_field_read(builder, ctx, member)
        }
        ChainElement::TSNonNullExpression(expr) => {
            lower_return_expression(builder, ctx, &expr.expression)
        }
    };
    ctx.exit_optional_chain();
    inner?;

    builder
        .emit_jump_to(Opcode::Jump, end_label)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Jump (chain end): {err:?}"))
        })?;
    builder.bind_label(short_circuit).map_err(|err| {
        SourceLoweringError::Internal(format!("bind chain short-circuit: {err:?}"))
    })?;
    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!(
            "encode LdaUndefined (chain short-circuit): {err:?}"
        ))
    })?;
    builder
        .bind_label(end_label)
        .map_err(|err| SourceLoweringError::Internal(format!("bind chain end: {err:?}")))?;
    Ok(())
}

pub(super) fn lower_return_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a Expression<'a>,
) -> Result<(), SourceLoweringError> {
    match expr {
        Expression::Identifier(ident) => lower_identifier_reference(builder, ctx, ident),
        Expression::NumericLiteral(literal) => {
            // Fast path: int32-fit integers go through `LdaSmi`.
            // Anything fractional / out of range (3.14, 1e20, NaN,
            // Infinity via `1/0`) interns the f64 and emits
            // `LdaConstF64` — no more "non_int32_literal" rejection.
            if let Ok(value) = int32_from_literal(literal) {
                builder
                    .emit(Opcode::LdaSmi, &[Operand::Imm(value)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode LdaSmi: {err:?}"))
                    })?;
            } else {
                let idx = ctx.intern_float_constant(literal.value)?;
                builder
                    .emit(Opcode::LdaConstF64, &[Operand::Idx(idx)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode LdaConstF64: {err:?}"))
                    })?;
            }
            Ok(())
        }
        Expression::NullLiteral(_) => {
            builder
                .emit(Opcode::LdaNull, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaNull: {err:?}")))?;
            Ok(())
        }
        Expression::BooleanLiteral(lit) => {
            let opcode = if lit.value {
                Opcode::LdaTrue
            } else {
                Opcode::LdaFalse
            };
            builder
                .emit(opcode, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaBool: {err:?}")))?;
            Ok(())
        }
        Expression::StringLiteral(lit) => {
            // M15: intern the literal's UTF-8 value into the
            // function's string-literal side table and emit
            // `LdaConstStr <idx>`. The interpreter materialises a
            // runtime-owned `JsString` on demand (§6.1.4).
            let idx = ctx.intern_string_literal(lit.value.as_str())?;
            builder
                .emit(Opcode::LdaConstStr, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaConstStr: {err:?}"))
                })?;
            Ok(())
        }
        // M36: §6.1.6.2 BigInt literal — `42n`. oxc provides
        // the value already normalised to base-10 without the
        // trailing `n` suffix, which matches what
        // `alloc_bigint` expects.
        Expression::BigIntLiteral(lit) => {
            let idx = ctx.intern_bigint_literal(lit.value.as_str())?;
            builder
                .emit(Opcode::LdaConstBigInt, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaConstBigInt: {err:?}"))
                })?;
            Ok(())
        }
        // M36: §22.2 RegExp literal — `/pattern/flags` records
        // the source form into the function's regexp table and
        // emits `CreateRegExp`. Each evaluation allocates a
        // fresh RegExp object (§22.2.1.5) so there's no dedup.
        Expression::RegExpLiteral(lit) => {
            let pattern = lit.regex.pattern.text.as_str();
            let flags = lit.regex.flags.to_string();
            // §22.2 early error: invalid RegExp pattern/flags must throw
            // a parse-phase SyntaxError (not a runtime error when the
            // literal is evaluated). Without this check, patterns such
            // as `\p{ASCII=T}` or `\u{FFFF}` with invalid flag combos
            // are only rejected at first `exec` / `test` call — test262
            // harness detects the late throw and fails.
            if let Err(err) = regress::Regex::with_flags(pattern, flags.as_str()) {
                return Err(SourceLoweringError::Parse {
                    message: format!("Invalid regular expression: {err}"),
                    span: lit.span,
                });
            }
            let idx = ctx.push_regexp_literal(pattern, &flags)?;
            builder
                .emit(Opcode::CreateRegExp, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode CreateRegExp: {err:?}"))
                })?;
            Ok(())
        }
        Expression::BinaryExpression(binary) => lower_binary_expression(builder, ctx, binary),
        Expression::AssignmentExpression(assign) => {
            // Nested assignment (`return x = 5;`, `let y = x = 5;`).
            // The lowering leaves the assigned value in acc, so this
            // composes as a normal accumulator-producing expression.
            lower_assignment_expression(builder, ctx, assign)
        }
        Expression::CallExpression(call) => {
            // `return f(args)`, `let x = f(args)`, `if (f(args))`,
            // any acc-producing position. Result lands in the
            // accumulator after `CallDirect`.
            lower_call_expression(builder, ctx, call)
        }
        Expression::ParenthesizedExpression(inner) => {
            lower_return_expression(builder, ctx, &inner.expression)
        }
        Expression::UnaryExpression(unary) => lower_unary_expression(builder, ctx, unary),
        Expression::UpdateExpression(update) => lower_update_expression(builder, ctx, update),
        Expression::ConditionalExpression(cond) => lower_conditional_expression(builder, ctx, cond),
        Expression::LogicalExpression(logical) => lower_logical_expression(builder, ctx, logical),
        Expression::ObjectExpression(obj) => lower_object_expression(builder, ctx, obj),
        Expression::ArrayExpression(arr) => lower_array_expression(builder, ctx, arr),
        Expression::StaticMemberExpression(member) => {
            lower_static_member_read(builder, ctx, member)
        }
        Expression::ComputedMemberExpression(member) => {
            lower_computed_member_read(builder, ctx, member)
        }
        // M29: `obj.#x` — §13.3.2 PrivateFieldExpression read.
        // Private-name resolution checks the enclosing class
        // body's declaration list at compile time; the runtime
        // walks `[[PrivateElements]]` using the active closure's
        // `class_id`.
        Expression::PrivateFieldExpression(expr) => lower_private_field_read(builder, ctx, expr),
        // M29: `#name in obj` — §13.10.1 PrivateInExpression.
        // Evaluates the RHS into a temp, then `InPrivate` checks
        // the runtime's `[[PrivateElements]]` table against the
        // active class_id.
        Expression::PrivateInExpression(expr) => lower_private_in_expression(builder, ctx, expr),
        // M33: `await <expr>` — lowers the operand into acc then
        // emits the `Await` opcode. Runtime semantics: drain the
        // microtask queue, unwrap settled promises (or throw on
        // rejection), pass plain values through unchanged per
        // §27.7.5.3 step 5.
        Expression::AwaitExpression(await_expr) => {
            lower_return_expression(builder, ctx, &await_expr.argument)?;
            builder
                .emit(Opcode::Await, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Await: {err:?}")))?;
            Ok(())
        }
        // M34: `yield <expr>` — §14.4 YieldExpression. Lowers
        // the operand into acc, emits `Yield` (suspends the
        // generator, returns to the `.next()` caller with
        // `{ value: acc, done: false }`). On resume, acc carries
        // the caller-provided sent value.
        //
        // `yield*` delegation (`expr.delegate`) is a separate
        // AST shape and stays deferred to a follow-up.
        Expression::YieldExpression(yield_expr) => {
            if yield_expr.delegate {
                return lower_yield_star(builder, ctx, yield_expr);
            }
            if let Some(arg) = yield_expr.argument.as_ref() {
                lower_return_expression(builder, ctx, arg)?;
            } else {
                builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaUndefined (yield): {err:?}"))
                })?;
            }
            builder
                .emit(Opcode::Yield, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Yield: {err:?}")))?;
            Ok(())
        }
        // TS-only non-null assertion. Runtime semantics are a
        // no-op; just lower the wrapped expression.
        Expression::TSNonNullExpression(expr) => {
            lower_return_expression(builder, ctx, &expr.expression)
        }
        // §13.3.9 Optional Chains — `o?.a`, `o?.[k]`, `f?.()`,
        // and any composition thereof. The ChainExpression wraps
        // the whole chain; individual optional elements inside it
        // carry `optional: true` and short-circuit to a shared
        // label that the chain's end installs.
        Expression::ChainExpression(chain) => lower_chain_expression(builder, ctx, chain),
        Expression::TemplateLiteral(tpl) => lower_template_literal(builder, ctx, tpl),
        // §13.3.11 `` tag`...${x}...` `` — call `tag(strings,
        // ...values)` where `strings` is the cooked-parts array
        // with a `.raw` property pointing at the raw-parts array.
        Expression::TaggedTemplateExpression(tagged) => {
            lower_tagged_template_expression(builder, ctx, tagged)
        }
        Expression::FunctionExpression(func) => lower_function_expression(builder, ctx, func),
        Expression::ArrowFunctionExpression(arrow) => {
            lower_arrow_function_expression(builder, ctx, arrow)
        }
        // M27: `this` reads the function's receiver slot. Only
        // meaningful inside constructors and methods — in plain
        // function bodies `CallUndefinedReceiver` sets `this =
        // undefined` (non-strict mode).
        Expression::ThisExpression(_) => {
            builder
                .emit(Opcode::LdaThis, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaThis: {err:?}")))?;
            Ok(())
        }
        // M27: `new Foo(args)`. Flows through the `Construct`
        // opcode which allocates the receiver from
        // `Foo.prototype`, invokes the constructor with
        // `this = receiver`, and applies §9.2.2.1's return
        // override.
        Expression::NewExpression(new_expr) => lower_new_expression(builder, ctx, new_expr),
        // M27: `class { … }` / `class Foo { … }` as an expression —
        // lowers to the constructor value in acc. No outer binding
        // is created; callers consume the value directly (e.g. `let
        // C = class {…}` or `return class {…};`).
        Expression::ClassExpression(class) => lower_class_expression(builder, ctx, class),
        // M35: §13.3.10 `import(expr)` — evaluate the specifier
        // into a fresh temp, then emit `DynamicImport <reg>`. The
        // dispatch handler resolves+loads the module and returns
        // a fulfilled Promise of its namespace.
        Expression::ImportExpression(import) => {
            let temp = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                lower_return_expression(builder, ctx, &import.source)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (dynamic import spec): {err:?}"
                        ))
                    })?;
                builder
                    .emit(Opcode::DynamicImport, &[Operand::Reg(u32::from(temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode DynamicImport: {err:?}"))
                    })?;
                Ok(())
            })();
            ctx.release_temps(1);
            lower
        }
        // M35: `import.meta` — fetch the module-meta namespace
        // from the runtime. Our current slice exposes a plain
        // object with one `url` string property.
        Expression::MetaProperty(meta)
            if meta.meta.name.as_str() == "import" && meta.property.name.as_str() == "meta" =>
        {
            builder.emit(Opcode::ImportMeta, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode ImportMeta: {err:?}"))
            })?;
            Ok(())
        }
        // §13.3.12 `new.target` — inside a [[Construct]] call the
        // expression yields the constructor that was invoked via
        // `new`; in an ordinary call it yields `undefined`.
        // `LdaNewTarget` already reads the active frame's slot, so
        // the compiler just needs to emit it.
        Expression::MetaProperty(meta)
            if meta.meta.name.as_str() == "new" && meta.property.name.as_str() == "target" =>
        {
            builder.emit(Opcode::LdaNewTarget, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaNewTarget: {err:?}"))
            })?;
            Ok(())
        }
        // §13.16 Comma Operator — `(a, b, c)`. Evaluate each sub-
        // expression left-to-right, discarding the accumulator value
        // of all but the final one, which becomes the expression's
        // completion. The parser guarantees at least two expressions;
        // we defensively pass a single-element sequence straight
        // through to the inner lowering so degenerate parser output
        // stays harmless.
        Expression::SequenceExpression(seq) => {
            let Some((last, head)) = seq.expressions.split_last() else {
                return Err(SourceLoweringError::Internal(
                    "SequenceExpression with no expressions".into(),
                ));
            };
            for discarded in head {
                lower_return_expression(builder, ctx, discarded)?;
            }
            lower_return_expression(builder, ctx, last)
        }
        other => Err(SourceLoweringError::unsupported(
            expression_construct_tag(other),
            other.span(),
        )),
    }
}

/// Lowers an expression into the accumulator. This is the same
/// surface as [`lower_return_expression`] — the helper exists as an
/// alias kept for the binary/relational-LHS call sites so future
/// readers see "the LHS lowers via the standard expression path"
/// rather than chasing through `lower_return_expression`.
///
/// Accepting binary and assignment expressions on the LHS unlocks
/// the bench2 idiom `(s + i) | 0`: the parenthesised binary lowers
/// into acc cleanly (binary operations always produce their result
/// in acc), and the outer `| 0` then operates against that acc.
pub(super) fn lower_accumulator_operand(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    lower_return_expression(builder, ctx, expr)
}
