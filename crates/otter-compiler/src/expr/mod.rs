//! Expression-level bytecode lowering and numeric coercion helpers.
//!
//! # Contents
//! - [`compile_expr`] — main expression dispatch.
//! - [`compile_expr_as_property_key`] — property-key coercion for patterns.
//! - [`coerce_compound_operands`] — compound assignment operand coercion.
//! - [`emit_to_primitive`] — ToPrimitive bytecode emission helper.
//! - [`identifier`] — identifier expression lowering.
//! - [`literal`] — literal expression lowering.
//! - [`unary`] — unary and update expression lowering.
//! - [`binary`] — binary, logical, and private-in lowering.
//! - [`member`] — member and private-field access lowering.
//! - [`construct`] — `new` expression lowering.
//! - [`object_array`] — object and array literal lowering.
//! - [`async_ops`] — `await` and `yield` lowering.
//! - [`import_meta`] — `import.meta`, `new.target`, and dynamic import lowering.
//!
//! # Invariants
//! - Expression lowering writes its result to the requested destination register.
//!
//! # See also
//! - `statements`, `calls`, and `class`

mod async_ops;
mod binary;
mod construct;
pub(crate) mod identifier;
mod import_meta;
mod literal;
mod member;
mod object_array;
mod unary;

use crate::*;

pub(crate) fn compile_expr(
    cx: &mut Compiler,
    expr: &Expression<'_>,
    enclosing_span: (u32, u32),
) -> Result<u16, CompileError> {
    let expr = unwrap_ts_expr(expr);
    match expr {
        // §9.1.1.2.1 — an enclosing `with` may shadow `undefined`
        // (`with ({undefined: 1})`), so the constant fold only
        // applies outside `with` bodies.
        Expression::Identifier(id)
            if id.name.as_str() == "undefined" && cx.active_with_envs.is_empty() =>
        {
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(dst)], enclosing_span);
            Ok(dst)
        }

        // §19.1 `globalThis` — when the user hasn't shadowed the
        // name, return the runtime's per-Interpreter shared
        // globalThis JsObject.
        // <https://tc39.es/ecma262/#sec-globalthis>
        Expression::Identifier(id)
            if id.name.as_str() == "globalThis"
                && cx.active_with_envs.is_empty()
                && cx.lookup_binding("globalThis").is_none()
                && find_module_import_binding(cx, "globalThis").is_none() =>
        {
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadGlobalThis, [Operand::Register(dst)], enclosing_span);
            Ok(dst)
        }

        Expression::NullLiteral(lit) => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadNull,
                [Operand::Register(dst)],
                (lit.span.start, lit.span.end),
            );
            Ok(dst)
        }

        Expression::ThisExpression(t) => {
            let span = (t.span.start, t.span.end);
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(dst)], span);
            Ok(dst)
        }

        Expression::Super(s) => {
            // Bare `super` standalone is a SyntaxError in real JS;
            // the grammar only accepts it as a call target or as
            // the object of a member expression. We surface a
            // friendly compile-time diagnostic so the rejection
            // happens at the right layer.
            Err(CompileError::Unsupported {
                node: "Super: bare `super` outside call or member expression".to_string(),
                span: (s.span.start, s.span.end),
            })
        }

        Expression::Identifier(id) => identifier::compile_identifier(cx, id, enclosing_span),

        Expression::LogicalExpression(l) => binary::compile_logical(cx, l, enclosing_span),

        Expression::ConditionalExpression(c) => {
            let span = (c.span.start, c.span.end);
            let cond = compile_expr(cx, &c.test, span)?;
            let dst = cx.alloc_scratch();
            let to_alt = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond), span);
            let cons = compile_expr(cx, &c.consequent, span)?;
            cx.emit(
                Op::StoreLocal,
                [Operand::Register(cons), Operand::Imm32(dst as i32)],
                span,
            );
            let to_end = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch_to_here(to_alt);
            let alt = compile_expr(cx, &c.alternate, span)?;
            cx.emit(
                Op::StoreLocal,
                [Operand::Register(alt), Operand::Imm32(dst as i32)],
                span,
            );
            cx.patch_branch_to_here(to_end);
            let out = cx.alloc_scratch();
            cx.emit(
                Op::LoadLocal,
                [Operand::Register(out), Operand::Imm32(dst as i32)],
                span,
            );
            Ok(out)
        }

        Expression::AssignmentExpression(a) => compile_assignment(cx, a),

        Expression::StringLiteral(lit) => literal::compile_string_literal(cx, lit, enclosing_span),

        Expression::BigIntLiteral(lit) => literal::compile_bigint_literal(cx, lit, enclosing_span),

        Expression::RegExpLiteral(lit) => literal::compile_regexp_literal(cx, lit, enclosing_span),

        Expression::NumericLiteral(lit) => {
            literal::compile_numeric_literal(cx, lit, enclosing_span)
        }

        Expression::BooleanLiteral(lit) => {
            literal::compile_boolean_literal(cx, lit, enclosing_span)
        }

        Expression::UnaryExpression(u) => unary::compile_unary(cx, u, enclosing_span),

        // §13.16 — `(a, b, c)`. Evaluate each in order, return the
        // last value.
        // <https://tc39.es/ecma262/#sec-comma-operator>
        Expression::SequenceExpression(s) => {
            let span = (s.span.start, s.span.end);
            let mut last = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(last)], span);
            for expr in s.expressions.iter() {
                last = compile_expr(cx, expr, span)?;
            }
            Ok(last)
        }

        Expression::TemplateLiteral(t) => compile_template_literal(cx, t),

        // §13.3.11 TaggedTemplate — `tag` call with `(strings, ...exprs)`.
        // <https://tc39.es/ecma262/#sec-tagged-templates>
        Expression::TaggedTemplateExpression(t) => compile_tagged_template(cx, t),

        // §13.3.9 Optional Chaining (`a?.b`, `a?.[k]`, `a?.()`).
        // <https://tc39.es/ecma262/#sec-optional-chains>
        Expression::ChainExpression(c) => compile_chain_expression(cx, c),

        // §13.3.7 PrivateFieldExpression — `obj.#name`.
        // <https://tc39.es/ecma262/#sec-makeprivatereference>
        Expression::PrivateFieldExpression(p) => {
            member::compile_private_field(cx, p, enclosing_span)
        }

        // §13.10.1 — `#name in obj` private-name membership probe.
        // <https://tc39.es/ecma262/#sec-relational-operators-runtime-semantics-evaluation>
        Expression::PrivateInExpression(p) => binary::compile_private_in(cx, p, enclosing_span),

        Expression::BinaryExpression(b) => binary::compile_binary(cx, b, enclosing_span),

        Expression::StaticMemberExpression(m) => {
            member::compile_static_member(cx, m, enclosing_span)
        }

        // `s[i]` — runtime checks that `s` is a string.
        Expression::ComputedMemberExpression(m) => {
            member::compile_computed_member(cx, m, enclosing_span)
        }

        // `recv.method(arg0, arg1, ...)` — dispatched through the
        // builtin/native method path at run time.
        Expression::CallExpression(call) => compile_method_call(cx, call),

        // `new Callee(args...)` — emits `Op::New`. The runtime
        // allocates the receiver and links its prototype. The
        // built-in `Error` constructor keeps a fast lowering path
        // since it doesn't need a `prototype` chain to work.
        Expression::NewExpression(new_expr) => construct::compile_new(cx, new_expr, enclosing_span),

        Expression::ParenthesizedExpression(p) => {
            compile_expr(cx, &p.expression, (p.span.start, p.span.end))
        }

        Expression::ArrayExpression(arr) => {
            object_array::compile_array_literal(cx, arr, enclosing_span)
        }

        Expression::ObjectExpression(obj) => {
            object_array::compile_object_literal(cx, obj, enclosing_span)
        }

        Expression::FunctionExpression(f) => {
            let span = (f.span.start, f.span.end);
            let name =
                f.id.as_ref()
                    .map(|id| id.name.as_str().to_string())
                    .unwrap_or_else(|| "<anonymous>".to_string());
            // §10.2.11 — a NAMED function expression's self-name
            // binding is immutable inside the body.
            cx.fn_self_immutable_hint = f.id.is_some();
            let (function_id, captures) = compile_function_full(
                cx,
                &name,
                &f.params,
                &f.body,
                span,
                f.r#async,
                f.generator,
                false,
            )?;
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_function_id(function_id);
            emit_make_callable(cx, dst, const_idx, &captures, false, span)?;
            Ok(dst)
        }

        Expression::ArrowFunctionExpression(a) => {
            let span = (a.span.start, a.span.end);
            let (function_id, captures) = compile_arrow_function(cx, a, span)?;
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_function_id(function_id);
            emit_make_callable(cx, dst, const_idx, &captures, true, span)?;
            Ok(dst)
        }

        Expression::ClassExpression(class) => {
            let name = class.id.as_ref().map(|id| id.name.as_str().to_string());
            compile_class(cx, class, name.as_deref())
        }

        Expression::MetaProperty(meta) => {
            import_meta::compile_meta_property(cx, meta, enclosing_span)
        }

        Expression::ImportExpression(imp) => import_meta::compile_import(cx, imp, enclosing_span),

        Expression::AwaitExpression(a) => async_ops::compile_await(cx, a, enclosing_span),

        // §15.5 — `yield expr` inside a generator body. Lowered to
        // [`Op::Yield`]; the result register receives whatever value
        // the next `.next(arg)` call passes back in. `yield*` is
        // not yet implemented and surfaces as `Unsupported`.
        // <https://tc39.es/ecma262/#sec-yield>
        Expression::YieldExpression(y) => async_ops::compile_yield(cx, y, enclosing_span),

        // §13.4 Postfix / Prefix update — `i++` / `++i` / `i--` /
        // `--i`. Foundation handles Identifier targets only; member
        // and computed-member operands fall through to Unsupported
        // (a subsequent slice covers them when test262 surfaces a
        // matching gap).
        // <https://tc39.es/ecma262/#sec-update-expressions>
        Expression::UpdateExpression(u) => unary::compile_update(cx, u, enclosing_span),

        other => Err(CompileError::Unsupported {
            node: format!("Expression ({})", expr_kind_name(other)),
            span: expr_span(other),
        }),
    }
}

pub(crate) fn compile_expr_with_inferred_name(
    cx: &mut Compiler,
    expr: &Expression<'_>,
    inferred_name: &str,
    enclosing_span: (u32, u32),
) -> Result<u16, CompileError> {
    let expr = unwrap_ts_expr(expr);
    match expr {
        Expression::ParenthesizedExpression(p) => compile_expr_with_inferred_name(
            cx,
            &p.expression,
            inferred_name,
            (p.span.start, p.span.end),
        ),
        Expression::FunctionExpression(f) if f.id.is_none() => {
            let span = (f.span.start, f.span.end);
            let (function_id, captures) = compile_function_full(
                cx,
                inferred_name,
                &f.params,
                &f.body,
                span,
                f.r#async,
                f.generator,
                false,
            )?;
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_function_id(function_id);
            emit_make_callable(cx, dst, const_idx, &captures, false, span)?;
            Ok(dst)
        }
        Expression::ArrowFunctionExpression(a) => {
            let span = (a.span.start, a.span.end);
            let (function_id, captures) = compile_arrow_function(cx, a, span)?;
            {
                let module = Rc::clone(&cx.top_mut().module);
                module.borrow_mut().functions[function_id as usize].name =
                    inferred_name.to_string();
            }
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_function_id(function_id);
            emit_make_callable(cx, dst, const_idx, &captures, true, span)?;
            Ok(dst)
        }
        Expression::ClassExpression(class) if class.id.is_none() => {
            compile_class(cx, class, Some(inferred_name))
        }
        _ => compile_expr(cx, expr, enclosing_span),
    }
}

/// Lower a non-static `PropertyKey` to a register holding the
/// runtime key value. Used by destructuring patterns when the
/// key is a computed expression or a primitive literal that we
/// need at runtime (e.g. for delete in object-rest exclusion).
pub(crate) fn compile_expr_as_property_key(
    cx: &mut Compiler,
    key: &oxc_ast::ast::PropertyKey<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    use oxc_ast::ast::PropertyKey;
    if let Some(expr) = key.as_expression() {
        return compile_expr(cx, expr, span);
    }
    match key {
        PropertyKey::StaticIdentifier(id) => {
            let r = cx.alloc_scratch();
            let s = cx.intern_string_constant(id.name.as_str());
            cx.emit(
                Op::LoadString,
                [Operand::Register(r), Operand::ConstIndex(s)],
                span,
            );
            Ok(r)
        }
        PropertyKey::PrivateIdentifier(_) => Err(CompileError::Unsupported {
            node: "PrivateIdentifier as property key in pattern".to_string(),
            span,
        }),
        _ => Err(CompileError::Unsupported {
            node: format!("PropertyKey ({key:?}) in pattern"),
            span,
        }),
    }
}

/// Pre-coerce the loaded current value and RHS register of a compound
/// assignment through `Op::ToPrimitive`, mirroring the
/// [`Expression::BinaryExpression`] lowering for the equivalent
/// operator.
///
/// Compound assignment is specified as `x op= y` ⇒ `x = x op y`,
/// so the operand-coercion rules are identical to the plain
/// `BinaryExpression` rules (§13.15.4 step 1, §13.15.3
/// ApplyStringOrNumericBinaryOperator, §7.2.13 / §7.2.14 for the
/// relational and equality coercion ladders). Without this pass the
/// runtime sees a raw object operand (e.g. `new Boolean(true)`
/// receiver of `x ^= true`) and bails out of the type-checked numeric
/// opcode with `TypeMismatch`.
///
/// The `Op::ToPrimitive` runtime helper short-circuits on already
/// primitive operands, so the extra instruction is cheap on the
/// common path.
pub(crate) fn coerce_compound_operands(
    cx: &mut Compiler,
    op: Op,
    current: u16,
    rhs: u16,
    span: (u32, u32),
) -> (u16, u16) {
    let hint = match op {
        Op::Add => Some("default"),
        Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Pow
        | Op::BitwiseAnd
        | Op::BitwiseOr
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::Ushr => Some("number"),
        _ => None,
    };
    match hint {
        Some(h) => (
            emit_to_primitive(cx, current, h, span),
            emit_to_primitive(cx, rhs, h, span),
        ),
        None => (current, rhs),
    }
}

/// Emit `Op::ToPrimitive(hint)` reading from `src_reg` and writing
/// into a fresh scratch register; return the scratch register.
///
/// Used by the `+` lowering path to satisfy §13.15.4
/// `ApplyStringOrNumericBinaryOperator` step 1: both operands must
/// pass through `ToPrimitive(default)` before the runtime decides
/// between string concat and numeric add. The runtime fast-path
/// short-circuits on already-primitive values, so the extra
/// instruction is cheap.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-toprimitive>
pub(crate) fn emit_to_primitive(
    cx: &mut Compiler,
    src_reg: u16,
    hint: &str,
    span: (u32, u32),
) -> u16 {
    let dst = cx.alloc_scratch();
    let hint_idx = cx.intern_string_constant(hint);
    cx.emit(
        Op::ToPrimitive,
        vec![
            Operand::Register(dst),
            Operand::Register(src_reg),
            Operand::ConstIndex(hint_idx),
        ],
        span,
    );
    dst
}
