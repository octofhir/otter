//! Member expression lowering.
//!
//! # Contents
//! - [`compile_static_member`] — lowers named member reads.
//! - [`compile_computed_member`] — lowers computed member reads.
//! - [`compile_private_field`] — lowers private field reads.
//!
//! # Invariants
//! - Ordinary member reads evaluate their live receiver and property. Source
//!   spelling never replaces a mutable global binding or descriptor.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::{ComputedMemberExpression, PrivateFieldExpression, StaticMemberExpression};

pub(crate) fn compile_private_field(
    cx: &mut Compiler,
    p: &PrivateFieldExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let pspan = (p.span.start, p.span.end);
    let obj_reg = compile_expr(cx, &p.object, pspan)?;
    crate::class::emit_private_method_brand_check(cx, obj_reg, p.field.name.as_str(), pspan)?;
    let key_reg = crate::class::load_private_key(cx, p.field.name.as_str(), pspan)?;
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::PrivateGet,
        [
            Operand::Register(dst),
            Operand::Register(obj_reg),
            Operand::Register(key_reg),
        ],
        pspan,
    );
    Ok(dst)
}

pub(crate) fn compile_static_member(
    cx: &mut Compiler,
    m: &StaticMemberExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    // General named member access. The runtime resolves
    // `string.length` as the special-case length getter and
    // walks `JsObject` properties for objects.
    let span = (m.span.start, m.span.end);
    // `super.x` reads the parent prototype's property — the
    // runtime walks one hop up `%home`'s prototype
    // chain. Only valid inside a class method.
    if matches!(m.object, Expression::Super(_)) {
        return compile_super_member_load(cx, m.property.name.as_str(), span);
    }
    let mark = cx.scratch;
    // A receiver held in its own frame slot is read in place: nothing runs
    // between evaluating it and the load, and the load writes a fresh
    // register, never the binding.
    let receiver = match crate::expr::binary::borrowed_operand(cx, &m.object) {
        Some(reg) => reg,
        None => compile_expr(cx, &m.object, span)?,
    };
    let name_idx = cx.intern_string_constant(m.property.name.as_str());
    cx.reset_scratch(mark);
    let dst = cx.alloc_scratch();
    crate::type_hints::mark_class_receiver(cx, &m.object);
    cx.emit(
        Op::LoadProperty,
        [
            Operand::Register(dst),
            Operand::Register(receiver),
            Operand::ConstIndex(name_idx),
        ],
        span,
    );
    Ok(dst)
}

pub(crate) fn compile_computed_member(
    cx: &mut Compiler,
    m: &ComputedMemberExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (m.span.start, m.span.end);
    // `super[expr]` — load through `Object.getPrototypeOf(home)`
    // so the read picks up the parent prototype's slot per
    // §13.3.5 MakeSuperPropertyReference.
    if matches!(m.object, Expression::Super(_)) {
        // §13.3.5 MakeSuperPropertyReference — `super[key]` resolves
        // against `Object.getPrototypeOf(home)` but runs accessor
        // getters with the current frame's `this` as the receiver.
        let home_reg = load_synthetic_capture(cx, super_home_binding_name(cx), span)?;
        // §13.3.7.1 step 2 — `GetThisBinding` runs before the key
        // expression is evaluated. A `LoadThis` here surfaces the
        // derived-constructor TDZ ReferenceError before any side
        // effects in the key expression (e.g. `super[super()]`).
        let this_guard = cx.alloc_scratch();
        cx.emit_load_this(this_guard, span);
        let idx = compile_expr(cx, &m.expression, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadSuperElement,
            [
                Operand::Register(dst),
                Operand::Register(home_reg),
                Operand::Register(idx),
            ],
            span,
        );
        return Ok(dst);
    }
    let mark = cx.scratch;
    // Names held in their own frame slots are read in place: the key, last,
    // always; the receiver when the key cannot run code that reassigns it.
    let borrowed_key = crate::expr::binary::borrowed_operand(cx, &m.expression);
    let recv = match crate::expr::binary::borrowed_operand(cx, &m.object) {
        Some(reg)
            if borrowed_key.is_some()
                || crate::expr::binary::operand_is_effect_free(&m.expression) =>
        {
            reg
        }
        _ => compile_expr(cx, &m.object, span)?,
    };
    let idx = match borrowed_key {
        Some(reg) => reg,
        None => compile_expr(cx, &m.expression, span)?,
    };
    cx.reset_scratch(mark);
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::LoadElement,
        [
            Operand::Register(dst),
            Operand::Register(recv),
            Operand::Register(idx),
        ],
        span,
    );
    Ok(dst)
}
