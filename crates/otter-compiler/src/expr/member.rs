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
    let mangled = cx
        .mangle_private(p.field.name.as_str())
        .ok_or(CompileError::Unsupported {
            node: "PrivateFieldExpression outside any class body".to_string(),
            span: pspan,
        })?;
    let obj_reg = compile_expr(cx, &p.object, pspan)?;
    let name_idx = cx.intern_string_constant(&mangled);
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::LoadProperty,
        vec![
            Operand::Register(dst),
            Operand::Register(obj_reg),
            Operand::ConstIndex(name_idx),
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
    // `Symbol.<name>` — well-known symbol read. The runtime
    // resolves the name against the per-interpreter
    // well-known table (ECMA-262 §6.1.5.1).
    if let Expression::Identifier(id) = &m.object
        && id.name.as_str() == "Symbol"
    {
        let dst = cx.alloc_scratch();
        let name_idx = cx.intern_string_constant(m.property.name.as_str());
        cx.emit(
            Op::SymbolLoad,
            [Operand::Register(dst), Operand::ConstIndex(name_idx)],
            span,
        );
        return Ok(dst);
    }
    let receiver = compile_expr(cx, &m.object, span)?;
    let name_idx = cx.intern_string_constant(m.property.name.as_str());
    let dst = cx.alloc_scratch();
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
        let home_reg = load_synthetic_capture(cx, SUPER_HOME_NAME, span)?;
        let parent_reg = cx.alloc_scratch();
        cx.emit(
            Op::GetPrototype,
            [Operand::Register(parent_reg), Operand::Register(home_reg)],
            span,
        );
        let idx = compile_expr(cx, &m.expression, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadElement,
            vec![
                Operand::Register(dst),
                Operand::Register(parent_reg),
                Operand::Register(idx),
            ],
            span,
        );
        return Ok(dst);
    }
    let recv = compile_expr(cx, &m.object, span)?;
    let idx = compile_expr(cx, &m.expression, span)?;
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
