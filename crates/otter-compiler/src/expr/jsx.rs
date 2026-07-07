//! JSX expression lowering for the classic React runtime.
//!
//! JSX and TSX are parsed by `otter-syntax` through OXC. This module lowers the
//! resulting AST directly into ordinary bytecode, without re-emitting source:
//! elements become `React.createElement(type, props, ...children)`.
//!
//! # Contents
//! - Element and fragment lowering.
//! - JSX tag-name lowering (`"div"`, `Component`, `UI.Button`).
//! - Props object construction with ordered spread support.
//! - Child lowering for text, nested JSX, and expression containers.
//!
//! # Invariants
//! - Lowering is AST-first; no JSX source scanning or string transforms.
//! - The emitted call uses the current `React.createElement` value, so user
//!   bindings/imports/shadowing are observable.
//! - Unsupported JSX syntax returns `CompileError::Unsupported` with a source
//!   span instead of silently producing a partial transform.
//!
//! # See also
//! - [`super::compile_expr`]

use crate::*;
use oxc_ast::ast::{
    JSXAttributeItem, JSXAttributeName, JSXAttributeValue, JSXChild, JSXElement, JSXElementName,
    JSXExpression, JSXExpressionContainer, JSXFragment, JSXMemberExpression,
    JSXMemberExpressionObject,
};

pub(crate) fn compile_jsx_element(
    cx: &mut Compiler,
    element: &JSXElement<'_>,
) -> Result<u16, CompileError> {
    let span = (element.span.start, element.span.end);
    let tag = compile_jsx_element_name(cx, &element.opening_element.name, span)?;
    let props = compile_jsx_props(cx, &element.opening_element.attributes, span)?;
    let children = compile_jsx_children(cx, &element.children)?;
    emit_react_create_element_call(cx, tag, props, children, span)
}

pub(crate) fn compile_jsx_fragment(
    cx: &mut Compiler,
    fragment: &JSXFragment<'_>,
) -> Result<u16, CompileError> {
    let span = (fragment.span.start, fragment.span.end);
    let tag = compile_react_fragment(cx, span)?;
    let props = load_null(cx, span);
    let children = compile_jsx_children(cx, &fragment.children)?;
    emit_react_create_element_call(cx, tag, props, children, span)
}

fn emit_react_create_element_call(
    cx: &mut Compiler,
    tag: u16,
    props: u16,
    children: Vec<u16>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let react = crate::expr::identifier::compile_identifier_without_with(cx, "React", span)?;
    let create_element_name = cx.intern_string_constant("createElement");
    let create_element = cx.alloc_scratch();
    cx.emit(
        Op::LoadProperty,
        [
            Operand::Register(create_element),
            Operand::Register(react),
            Operand::ConstIndex(create_element_name),
        ],
        span,
    );

    let mut args = Vec::with_capacity(2 + children.len());
    args.push(tag);
    args.push(props);
    args.extend(children);
    check_call_arity(args.len(), "JSX React.createElement", span)?;

    let dst = cx.alloc_scratch();
    let mut operands = Vec::with_capacity(4 + args.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(create_element));
    operands.push(Operand::Register(react));
    operands.push(Operand::ConstIndex(args.len() as u32));
    operands.extend(args.into_iter().map(Operand::Register));
    cx.emit(Op::CallWithThis, operands, span);
    Ok(dst)
}

fn compile_react_fragment(cx: &mut Compiler, span: (u32, u32)) -> Result<u16, CompileError> {
    let react = crate::expr::identifier::compile_identifier_without_with(cx, "React", span)?;
    emit_load_property(cx, react, "Fragment", span)
}

fn compile_jsx_element_name(
    cx: &mut Compiler,
    name: &JSXElementName<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    match name {
        JSXElementName::Identifier(id) => compile_jsx_identifier_tag(cx, id.name.as_str(), span),
        JSXElementName::IdentifierReference(id) => {
            compile_jsx_identifier_tag(cx, id.name.as_str(), span)
        }
        JSXElementName::MemberExpression(member) => compile_jsx_member_expression(cx, member, span),
        JSXElementName::ThisExpression(_) => {
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(dst)], span);
            Ok(dst)
        }
        JSXElementName::NamespacedName(name) => Err(CompileError::Unsupported {
            node: format!(
                "JSX namespaced tag {}:{}",
                name.namespace.name.as_str(),
                name.name.name.as_str()
            ),
            span: (name.span.start, name.span.end),
        }),
    }
}

fn compile_jsx_identifier_tag(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if is_intrinsic_jsx_tag(name) {
        Ok(load_string(cx, name, span))
    } else {
        crate::expr::identifier::compile_identifier_without_with(cx, name, span)
    }
}

fn is_intrinsic_jsx_tag(name: &str) -> bool {
    name.contains('-')
        || name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
}

fn compile_jsx_member_expression(
    cx: &mut Compiler,
    member: &JSXMemberExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let object = match &member.object {
        JSXMemberExpressionObject::IdentifierReference(id) => {
            crate::expr::identifier::compile_identifier_without_with(cx, id.name.as_str(), span)?
        }
        JSXMemberExpressionObject::MemberExpression(inner) => {
            compile_jsx_member_expression(cx, inner, span)?
        }
        JSXMemberExpressionObject::ThisExpression(_) => {
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(dst)], span);
            dst
        }
    };
    emit_load_property(cx, object, member.property.name.as_str(), span)
}

fn compile_jsx_props(
    cx: &mut Compiler,
    attrs: &[JSXAttributeItem<'_>],
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if attrs.is_empty() {
        return Ok(load_null(cx, span));
    }
    let props = cx.alloc_scratch();
    cx.emit(Op::NewObject, [Operand::Register(props)], span);

    let attr_mark = cx.scratch;
    for attr in attrs {
        cx.reset_scratch(attr_mark);
        match attr {
            JSXAttributeItem::Attribute(attr) => {
                let attr_span = (attr.span.start, attr.span.end);
                let key = jsx_attribute_name(&attr.name)?;
                let value = match &attr.value {
                    None => {
                        let value = cx.alloc_scratch();
                        cx.emit(Op::LoadTrue, [Operand::Register(value)], attr_span);
                        value
                    }
                    Some(JSXAttributeValue::StringLiteral(lit)) => {
                        load_string(cx, lit.value.as_str(), (lit.span.start, lit.span.end))
                    }
                    Some(JSXAttributeValue::ExpressionContainer(container)) => {
                        compile_jsx_expression_container(cx, container)?
                    }
                    Some(JSXAttributeValue::Element(element)) => compile_jsx_element(cx, element)?,
                    Some(JSXAttributeValue::Fragment(fragment)) => {
                        compile_jsx_fragment(cx, fragment)?
                    }
                };
                emit_define_data_property(cx, props, &key, value, attr_span);
            }
            JSXAttributeItem::SpreadAttribute(spread) => {
                let spread_span = (spread.span.start, spread.span.end);
                let src = compile_expr(cx, &spread.argument, spread_span)?;
                cx.emit(
                    Op::CopyDataProperties,
                    [Operand::Register(props), Operand::Register(src)],
                    spread_span,
                );
            }
        }
    }

    Ok(props)
}

fn jsx_attribute_name(name: &JSXAttributeName<'_>) -> Result<String, CompileError> {
    match name {
        JSXAttributeName::Identifier(id) => Ok(id.name.to_string()),
        JSXAttributeName::NamespacedName(name) => Err(CompileError::Unsupported {
            node: format!(
                "JSX namespaced attribute {}:{}",
                name.namespace.name.as_str(),
                name.name.name.as_str()
            ),
            span: (name.span.start, name.span.end),
        }),
    }
}

fn compile_jsx_children(
    cx: &mut Compiler,
    children: &[JSXChild<'_>],
) -> Result<Vec<u16>, CompileError> {
    let mut out = Vec::new();
    for child in children {
        match child {
            JSXChild::Text(text) => {
                if let Some(value) = normalize_jsx_text(text.value.as_str()) {
                    out.push(load_string(cx, &value, (text.span.start, text.span.end)));
                }
            }
            JSXChild::Element(element) => out.push(compile_jsx_element(cx, element)?),
            JSXChild::Fragment(fragment) => out.push(compile_jsx_fragment(cx, fragment)?),
            JSXChild::ExpressionContainer(container) => {
                if let Some(value) = compile_optional_jsx_expression_container(cx, container)? {
                    out.push(value);
                }
            }
            JSXChild::Spread(spread) => {
                return Err(CompileError::Unsupported {
                    node: "JSX spread child".to_string(),
                    span: (spread.span.start, spread.span.end),
                });
            }
        }
    }
    Ok(out)
}

fn compile_jsx_expression_container(
    cx: &mut Compiler,
    container: &JSXExpressionContainer<'_>,
) -> Result<u16, CompileError> {
    compile_optional_jsx_expression_container(cx, container)?.ok_or_else(|| {
        CompileError::Unsupported {
            node: "empty JSX expression container".to_string(),
            span: (container.span.start, container.span.end),
        }
    })
}

fn compile_optional_jsx_expression_container(
    cx: &mut Compiler,
    container: &JSXExpressionContainer<'_>,
) -> Result<Option<u16>, CompileError> {
    match &container.expression {
        JSXExpression::EmptyExpression(_) => Ok(None),
        expression => {
            let Some(expression) = expression.as_expression() else {
                return Err(CompileError::Unsupported {
                    node: "JSX expression container".to_string(),
                    span: (container.span.start, container.span.end),
                });
            };
            Ok(Some(compile_expr(
                cx,
                expression,
                (container.span.start, container.span.end),
            )?))
        }
    }
}

fn normalize_jsx_text(raw: &str) -> Option<String> {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() { None } else { Some(text) }
}

fn load_string(cx: &mut Compiler, value: &str, span: (u32, u32)) -> u16 {
    let dst = cx.alloc_scratch();
    let idx = cx.intern_string_constant(value);
    cx.emit(
        Op::LoadString,
        [Operand::Register(dst), Operand::ConstIndex(idx)],
        span,
    );
    dst
}

fn load_null(cx: &mut Compiler, span: (u32, u32)) -> u16 {
    let dst = cx.alloc_scratch();
    cx.emit(Op::LoadNull, [Operand::Register(dst)], span);
    dst
}

fn emit_load_property(
    cx: &mut Compiler,
    receiver: u16,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let dst = cx.alloc_scratch();
    let name_idx = cx.intern_string_constant(name);
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

fn emit_define_data_property(
    cx: &mut Compiler,
    object: u16,
    key: &str,
    value: u16,
    span: (u32, u32),
) {
    let key_reg = load_string(cx, key, span);
    cx.emit(
        Op::DefineDataProperty,
        [
            Operand::Register(object),
            Operand::Register(key_reg),
            Operand::Register(value),
        ],
        span,
    );
}
