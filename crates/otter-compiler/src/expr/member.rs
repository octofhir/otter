//! Member expression lowering.
//!
//! # Contents
//! - [`compile_static_member`] — lowers named member reads.
//! - [`compile_computed_member`] — lowers computed member reads.
//! - [`compile_private_field`] — lowers private field reads.
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
        vec![
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
    // runtime walks one hop up `__class_home`'s prototype
    // chain. Only valid inside a class method.
    if matches!(m.object, Expression::Super(_)) {
        return compile_super_member_load(cx, m.property.name.as_str(), span);
    }
    // §23.2.5 TypedArray-constructor static properties:
    // `<T>.BYTES_PER_ELEMENT`. Lower the integer value at
    // compile time so the runtime does not need a real
    // constructor object.
    // <https://tc39.es/ecma262/#sec-typedarray.bytes_per_element>
    if let Expression::Identifier(id) = &m.object
        && is_typed_array_name(id.name.as_str())
        && m.property.name.as_str() == "BYTES_PER_ELEMENT"
        && cx.lookup_binding(id.name.as_str()).is_none()
        && find_module_import_binding(cx, id.name.as_str()).is_none()
    {
        let bpe: i32 = match id.name.as_str() {
            "Int8Array" | "Uint8Array" | "Uint8ClampedArray" => 1,
            "Int16Array" | "Uint16Array" => 2,
            "Int32Array" | "Uint32Array" | "Float32Array" => 4,
            _ => 8,
        };
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadInt32,
            [Operand::Register(dst), Operand::Imm32(bpe)],
            span,
        );
        return Ok(dst);
    }
    // §21.1.1.x Number static constants — `MAX_SAFE_INTEGER`
    // / `MIN_SAFE_INTEGER` / `MAX_VALUE` / `MIN_VALUE` /
    // `EPSILON` / `POSITIVE_INFINITY` / `NEGATIVE_INFINITY`
    // / `NaN`. Inline the literal value at compile time so
    // the runtime doesn't need a real `Number` global.
    // <https://tc39.es/ecma262/#sec-properties-of-the-number-constructor>
    if let Expression::Identifier(id) = &m.object
        && id.name.as_str() == "Number"
        && cx.lookup_binding("Number").is_none()
        && find_module_import_binding(cx, "Number").is_none()
        && let Some(value) = number_static_constant(m.property.name.as_str())
    {
        let dst = cx.alloc_scratch();
        let const_idx = cx.intern_number_constant(value);
        cx.emit(
            Op::LoadNumber,
            [Operand::Register(dst), Operand::ConstIndex(const_idx)],
            span,
        );
        return Ok(dst);
    }
    // `Math.PI` / `Math.E` / other value properties lower to
    // MathLoad. Method reads fall through to ordinary property
    // load now that task 96 installs a real `Math` namespace.
    if let Expression::Identifier(id) = &m.object
        && id.name.as_str() == "Math"
        && math_static_constant(m.property.name.as_str()).is_some()
    {
        let dst = cx.alloc_scratch();
        let name_idx = cx.intern_string_constant(m.property.name.as_str());
        cx.emit(
            Op::MathLoad,
            [Operand::Register(dst), Operand::ConstIndex(name_idx)],
            span,
        );
        return Ok(dst);
    }
    let mark = cx.scratch;
    let receiver = compile_expr(cx, &m.object, span)?;
    let name_idx = cx.intern_string_constant(m.property.name.as_str());
    cx.reset_scratch(mark);
    let dst = cx.alloc_scratch();
    crate::type_hints::mark_class_receiver(cx, &m.object);
    cx.emit(
        Op::LoadProperty,
        vec![
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
        cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
        let idx = compile_expr(cx, &m.expression, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadSuperElement,
            vec![
                Operand::Register(dst),
                Operand::Register(home_reg),
                Operand::Register(idx),
            ],
            span,
        );
        return Ok(dst);
    }
    let mark = cx.scratch;
    let recv = compile_expr(cx, &m.object, span)?;
    let idx = compile_expr(cx, &m.expression, span)?;
    cx.reset_scratch(mark);
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::LoadElement,
        vec![
            Operand::Register(dst),
            Operand::Register(recv),
            Operand::Register(idx),
        ],
        span,
    );
    Ok(dst)
}
