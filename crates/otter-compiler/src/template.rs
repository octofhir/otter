//! Template literal and tagged-template lowering helpers.
//!
//! # Contents
//! - template quasi interning
//! - template literal concatenation
//! - tagged template call assembly
//! - `String.raw` fast path
//!
//! # Invariants
//! - Template cooked and raw strings are interned through the module constant table.
//!
//! # See also
//! - `calls` and `expr`

use crate::*;

/// Lower a template literal `\`hello ${x} world\`` per §13.2.8 — a
/// sequence of `String` concats over cooked quasis and
/// interpolations.
///
/// # Algorithm
/// Per ECMA-262 §13.2.8.6:
/// 1. Evaluate `quasi[0].cooked` → result.
/// 2. For each expression `expr[i]`: `result = result + ToString(expr[i])`.
///    The runtime handles `ToString` via `Op::Add`'s string-or-numeric
///    ladder once `Op::ToPrimitive(default)` ran on each operand —
///    template-literal interpolations always produce strings, so the
///    `+` lowering works out of the box.
/// 3. After each interpolation, append `quasi[i+1].cooked`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-template-literals>
pub(crate) fn intern_template_quasi(
    cx: &mut Compiler,
    quasi: &oxc_ast::ast::TemplateElement<'_>,
) -> u32 {
    let cooked = quasi.value.cooked.as_deref().unwrap_or("");
    if quasi.lone_surrogates {
        cx.intern_utf16_string_constant(decode_lone_surrogate_string(cooked))
    } else {
        cx.intern_string_constant(cooked)
    }
}

pub(crate) fn compile_template_literal(
    cx: &mut Compiler,
    t: &oxc_ast::ast::TemplateLiteral<'_>,
) -> Result<u16, CompileError> {
    let span = (t.span.start, t.span.end);
    if t.expressions.is_empty() && t.quasis.len() == 1 {
        let dst = cx.alloc_scratch();
        let const_idx = intern_template_quasi(cx, &t.quasis[0]);
        cx.emit(
            Op::LoadString,
            [Operand::Register(dst), Operand::ConstIndex(const_idx)],
            span,
        );
        return Ok(dst);
    }
    // Seed with first cooked quasi.
    let mut acc = {
        let dst = cx.alloc_scratch();
        let const_idx = intern_template_quasi(cx, &t.quasis[0]);
        cx.emit(
            Op::LoadString,
            [Operand::Register(dst), Operand::ConstIndex(const_idx)],
            span,
        );
        dst
    };
    for (i, expr) in t.expressions.iter().enumerate() {
        let expr_reg = compile_expr(cx, expr, span)?;
        // §13.2.8.6 template interpolation applies ToString to each
        // substitution. `Op::Add` still performs the final string
        // concatenation, but object operands must enter the
        // ToPrimitive ladder with the string hint so ordinary
        // `toString` wins over `valueOf`.
        let lhs_in = emit_to_primitive(cx, acc, "default", span);
        let rhs_in = emit_to_primitive(cx, expr_reg, "string", span);
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::Add,
            vec![
                Operand::Register(dst),
                Operand::Register(lhs_in),
                Operand::Register(rhs_in),
            ],
            span,
        );
        acc = dst;
        // Append the next cooked quasi.
        let next_quasi = &t.quasis[i + 1];
        let cooked = next_quasi.value.cooked.as_deref().unwrap_or("");
        if !cooked.is_empty() {
            let quasi_reg = cx.alloc_scratch();
            let const_idx = intern_template_quasi(cx, next_quasi);
            cx.emit(
                Op::LoadString,
                [Operand::Register(quasi_reg), Operand::ConstIndex(const_idx)],
                span,
            );
            let lhs_in = emit_to_primitive(cx, acc, "default", span);
            let rhs_in = emit_to_primitive(cx, quasi_reg, "default", span);
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::Add,
                vec![
                    Operand::Register(dst),
                    Operand::Register(lhs_in),
                    Operand::Register(rhs_in),
                ],
                span,
            );
            acc = dst;
        }
    }
    Ok(acc)
}

/// Lower a tagged-template call: `tag\`...${a}...${b}...\`` per
/// ECMA-262 §13.3.11.4.
///
/// # Algorithm
/// 1. Build the `strings` array — `cooked` quasis in order. Attach
///    a `.raw` own property whose value is an array of the same
///    length holding the raw quasi text.
/// 2. Evaluate every interpolation expression, in source order.
/// 3. Call `tag(strings, ...exprs)` with `this = undefined` (foundation
///    matches the spec's `Reference` resolution; method-receiver
///    forms via `obj.tag\`...\`` are filed as a follow-up).
///
/// `strings.raw` is installed via `Op::StoreProperty` for foundation
/// fidelity; spec mandates the strings array be frozen and the `raw`
/// array be a separate own property — the foundation slice ships
/// the un-frozen shape and files freezing as a follow-up.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-tagged-templates>
/// - <https://tc39.es/ecma262/#sec-runtime-semantics-getemplateobject>
pub(crate) fn compile_tagged_template(
    cx: &mut Compiler,
    t: &oxc_ast::ast::TaggedTemplateExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (t.span.start, t.span.end);

    // §22.1.2.4 String.raw — recognise the literal call shape
    // `String.raw\`...\`` and inline the raw-text reconstruction.
    // Avoids the need for a real `String` namespace binding.
    // <https://tc39.es/ecma262/#sec-string.raw>
    if let Expression::StaticMemberExpression(member) = &t.tag
        && let Expression::Identifier(id) = &member.object
        && id.name.as_str() == "String"
        && member.property.name.as_str() == "raw"
        && cx.lookup_binding("String").is_none()
    {
        return compile_string_raw_template(cx, &t.quasi, span);
    }

    // §13.3.11.1 — a member-expression tag receives its base as
    // `this` (`obj.tag\`…\`` calls with this = obj).
    let (tag_reg, this_reg): (u16, Option<u16>) = match &t.tag {
        Expression::StaticMemberExpression(m) if !matches!(m.object, Expression::Super(_)) => {
            let obj_reg = compile_expr(cx, &m.object, span)?;
            let callee = cx.alloc_scratch();
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(callee),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            (callee, Some(obj_reg))
        }
        Expression::ComputedMemberExpression(m) if !matches!(m.object, Expression::Super(_)) => {
            let obj_reg = compile_expr(cx, &m.object, span)?;
            let key_reg = compile_expr(cx, &m.expression, span)?;
            let callee = cx.alloc_scratch();
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(callee),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            (callee, Some(obj_reg))
        }
        other => (compile_expr(cx, other, span)?, None),
    };

    // §13.2.8.4 GetTemplateObject — register this Parse Node as a
    // template site; the runtime caches the frozen strings object
    // per site so every evaluation hands the tag the SAME object.
    let site = otter_bytecode::TemplateSite {
        cooked: t
            .quasi
            .quasis
            .iter()
            .map(|q| q.value.cooked.as_ref().map(|c| c.to_string()))
            .collect(),
        raw: t
            .quasi
            .quasis
            .iter()
            .map(|q| q.value.raw.to_string())
            .collect(),
    };
    let site_idx = {
        let module = Rc::clone(&cx.top_mut().module);
        let mut m = module.borrow_mut();
        let idx = m.template_sites.len() as u32;
        m.template_sites.push(site);
        idx
    };
    let strings_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetTemplateObject,
        [
            Operand::Register(strings_reg),
            Operand::ConstIndex(site_idx),
        ],
        span,
    );

    // Evaluate interpolations.
    let mut arg_regs: Vec<u16> = Vec::with_capacity(1 + t.quasi.expressions.len());
    arg_regs.push(strings_reg);
    for expr in t.quasi.expressions.iter() {
        arg_regs.push(compile_expr(cx, expr, span)?);
    }

    // Emit `tag(strings, ...exprs)`. Dense `Op::Call` operand count
    // is bounded by `u8::MAX`; templates that interpolate hundreds
    // of expressions fall back to `Op::CallSpread` so the encoder
    // never panics.
    let dst = cx.alloc_scratch();
    if arg_regs.len() > DENSE_CALL_MAX_ARGS {
        let args_arr = cx.alloc_scratch();
        emit_array_from_regs(cx, args_arr, &arg_regs, span);
        let this_value = match this_reg {
            Some(r) => r,
            None => {
                let r = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
                r
            }
        };
        cx.emit(
            Op::CallSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(tag_reg),
                Operand::Register(this_value),
                Operand::Register(args_arr),
            ],
            span,
        );
    } else if let Some(this_reg) = this_reg {
        let mut call_operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        call_operands.push(Operand::Register(dst));
        call_operands.push(Operand::Register(tag_reg));
        call_operands.push(Operand::Register(this_reg));
        call_operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        call_operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, call_operands, span);
    } else {
        let mut call_operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
        call_operands.push(Operand::Register(dst));
        call_operands.push(Operand::Register(tag_reg));
        call_operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        call_operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::Call, call_operands, span);
    }
    Ok(dst)
}

/// Dense-form opcode operand cap. Mirrors
/// `compile_array_literal::DENSE_NEW_ARRAY_MAX_ELEMENTS`; keep the
/// two in sync.
const DENSE_CALL_MAX_ARGS: usize = 240;

/// Build a dense or per-element array from a slice of element
/// registers. Used by both the cooked / raw quasi arrays and the
/// `Op::CallSpread` fallback args bundle when the count crosses the
/// `u8::MAX` boundary on the dense `Op::NewArray` form.
fn emit_array_from_regs(cx: &mut Compiler, dst: u16, elements: &[u16], span: (u32, u32)) {
    if elements.len() <= DENSE_CALL_MAX_ARGS {
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + elements.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(elements.len() as u32));
        operands.extend(elements.iter().copied().map(Operand::Register));
        cx.emit(Op::NewArray, operands, span);
        return;
    }
    cx.emit(
        Op::NewArray,
        [Operand::Register(dst), Operand::ConstIndex(0)],
        span,
    );
    for &r in elements {
        cx.emit(
            Op::ArrayPush,
            [Operand::Register(dst), Operand::Register(r)],
            span,
        );
    }
}

/// Inline §22.1.2.4 `String.raw` for the tagged-template call shape.
/// Walks raw quasi text + interpolations, concatenating each.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-string.raw>
pub(crate) fn compile_string_raw_template(
    cx: &mut Compiler,
    quasi: &oxc_ast::ast::TemplateLiteral<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    // Seed accumulator with the first raw quasi.
    let mut acc = {
        let raw = quasi.quasis[0].value.raw.as_str();
        let dst = cx.alloc_scratch();
        let const_idx = cx.intern_string_constant(raw);
        cx.emit(
            Op::LoadString,
            [Operand::Register(dst), Operand::ConstIndex(const_idx)],
            span,
        );
        dst
    };
    for (i, expr) in quasi.expressions.iter().enumerate() {
        let expr_reg = compile_expr(cx, expr, span)?;
        let lhs_in = emit_to_primitive(cx, acc, "default", span);
        let rhs_in = emit_to_primitive(cx, expr_reg, "default", span);
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::Add,
            vec![
                Operand::Register(dst),
                Operand::Register(lhs_in),
                Operand::Register(rhs_in),
            ],
            span,
        );
        acc = dst;
        let raw = quasi.quasis[i + 1].value.raw.as_str();
        if !raw.is_empty() {
            let qr = cx.alloc_scratch();
            let const_idx = cx.intern_string_constant(raw);
            cx.emit(
                Op::LoadString,
                [Operand::Register(qr), Operand::ConstIndex(const_idx)],
                span,
            );
            let lhs_in = emit_to_primitive(cx, acc, "default", span);
            let rhs_in = emit_to_primitive(cx, qr, "default", span);
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::Add,
                vec![
                    Operand::Register(dst),
                    Operand::Register(lhs_in),
                    Operand::Register(rhs_in),
                ],
                span,
            );
            acc = dst;
        }
    }
    Ok(acc)
}
