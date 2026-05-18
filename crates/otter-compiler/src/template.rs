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
        // Mirror the BinaryExpression `+` lowering: pass each operand
        // through ToPrimitive(default) so `Op::Add`'s string-or-
        // numeric ladder fires correctly when an object exposes
        // `[Symbol.toPrimitive]` / `valueOf` / `toString`.
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

    let tag_reg = compile_expr(cx, &t.tag, span)?;

    // Build cooked + raw quasi arrays.
    let mut cooked_regs: Vec<u16> = Vec::with_capacity(t.quasi.quasis.len());
    let mut raw_regs: Vec<u16> = Vec::with_capacity(t.quasi.quasis.len());
    for q in t.quasi.quasis.iter() {
        let cooked = q.value.cooked.as_deref().unwrap_or("");
        let raw = q.value.raw.as_str();
        let cr = cx.alloc_scratch();
        let ci = cx.intern_string_constant(cooked);
        cx.emit(
            Op::LoadString,
            [Operand::Register(cr), Operand::ConstIndex(ci)],
            span,
        );
        let rr = cx.alloc_scratch();
        let ri = cx.intern_string_constant(raw);
        cx.emit(
            Op::LoadString,
            [Operand::Register(rr), Operand::ConstIndex(ri)],
            span,
        );
        cooked_regs.push(cr);
        raw_regs.push(rr);
    }

    // Materialise the cooked array.
    let strings_reg = cx.alloc_scratch();
    let mut cooked_operands: Vec<Operand> = Vec::with_capacity(2 + cooked_regs.len());
    cooked_operands.push(Operand::Register(strings_reg));
    cooked_operands.push(Operand::ConstIndex(cooked_regs.len() as u32));
    cooked_operands.extend(cooked_regs.iter().copied().map(Operand::Register));
    cx.emit(Op::NewArray, cooked_operands, span);

    // Materialise the raw array.
    let raw_arr_reg = cx.alloc_scratch();
    let mut raw_operands: Vec<Operand> = Vec::with_capacity(2 + raw_regs.len());
    raw_operands.push(Operand::Register(raw_arr_reg));
    raw_operands.push(Operand::ConstIndex(raw_regs.len() as u32));
    raw_operands.extend(raw_regs.iter().copied().map(Operand::Register));
    cx.emit(Op::NewArray, raw_operands, span);

    // Attach `strings.raw = raw_arr`.
    cx.emit_store_property(strings_reg, "raw", raw_arr_reg, span);

    // Evaluate interpolations.
    let mut arg_regs: Vec<u16> = Vec::with_capacity(1 + t.quasi.expressions.len());
    arg_regs.push(strings_reg);
    for expr in t.quasi.expressions.iter() {
        arg_regs.push(compile_expr(cx, expr, span)?);
    }

    // Emit `tag(strings, ...exprs)`.
    let dst = cx.alloc_scratch();
    let mut call_operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    call_operands.push(Operand::Register(dst));
    call_operands.push(Operand::Register(tag_reg));
    call_operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    call_operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::Call, call_operands, span);
    Ok(dst)
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
