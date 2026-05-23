//! Compiler-lowered builtin call emission helpers.
//!
//! # Contents
//! - error constructor fast paths
//!
//! # Invariants
//! - Fast paths are used only for recognized builtin shapes.
//!
//! # See also
//! - `builtins_table` for lookup predicates

use crate::*;

/// Lower `new <Kind>(arg)` / `<Kind>(arg)` for any of the seven
/// canonical native error classes to [`Op::NewBuiltinError`]. The
/// `Error` kind keeps the legacy [`Op::NewError`] lowering for
/// backwards compatibility with already-shipped fixtures.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
pub(crate) fn compile_builtin_error_construct(
    cx: &mut Compiler,
    kind: &str,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    // §20.5.7.1 — `new AggregateError(errors, message?, options?)`.
    // The first argument is the error iterable; the second is the
    // optional message; the third contributes `options.cause`.
    // Lower as `NewBuiltinError(message)` followed by own stores for
    // `errors` and, when present, `cause`.
    if kind == "AggregateError" {
        if arguments.len() > 3 {
            return Err(CompileError::Unsupported {
                node: format!("{kind}: more than three arguments"),
                span,
            });
        }
        let errors_reg = match arguments.first() {
            None => {
                let r = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
                r
            }
            Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
                return Err(CompileError::Unsupported {
                    node: format!("{kind}: spread argument"),
                    span: (s.span.start, s.span.end),
                });
            }
            Some(other) => compile_expr(cx, other.to_expression(), span)?,
        };
        let msg_reg = match arguments.get(1) {
            None => {
                let r = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
                r
            }
            Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
                return Err(CompileError::Unsupported {
                    node: format!("{kind}: spread argument"),
                    span: (s.span.start, s.span.end),
                });
            }
            Some(other) => compile_expr(cx, other.to_expression(), span)?,
        };
        let cause_reg = match arguments.get(2) {
            None => None,
            Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
                return Err(CompileError::Unsupported {
                    node: format!("{kind}: spread argument"),
                    span: (s.span.start, s.span.end),
                });
            }
            Some(other) => {
                let options_reg = compile_expr(cx, other.to_expression(), span)?;
                let cause_reg = cx.alloc_scratch();
                cx.emit_load_property(cause_reg, options_reg, "cause", span);
                Some(cause_reg)
            }
        };
        let dst = cx.alloc_scratch();
        let kind_idx = cx.intern_string_constant(kind);
        cx.emit(
            Op::NewBuiltinError,
            vec![
                Operand::Register(dst),
                Operand::ConstIndex(kind_idx),
                Operand::Register(msg_reg),
            ],
            span,
        );
        // Attach `errors` own property on the freshly built instance.
        let key_idx = cx.intern_string_constant("errors");
        let scratch = cx.alloc_scratch();
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(dst),
                Operand::ConstIndex(key_idx),
                Operand::Register(errors_reg),
                Operand::Register(scratch),
            ],
            span,
        );
        if let Some(cause_reg) = cause_reg {
            let key_idx = cx.intern_string_constant("cause");
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(dst),
                    Operand::ConstIndex(key_idx),
                    Operand::Register(cause_reg),
                    Operand::Register(scratch),
                ],
                span,
            );
        }
        return Ok(dst);
    }
    if arguments.len() > 2 {
        return Err(CompileError::Unsupported {
            node: format!("{kind}: more than two arguments"),
            span,
        });
    }
    let msg_reg = match arguments.first() {
        None => {
            let r = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
            r
        }
        Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
            return Err(CompileError::Unsupported {
                node: format!("{kind}: spread argument"),
                span: (s.span.start, s.span.end),
            });
        }
        Some(other) => compile_expr(cx, other.to_expression(), span)?,
    };
    let cause_reg = match arguments.get(1) {
        None => None,
        Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
            return Err(CompileError::Unsupported {
                node: format!("{kind}: spread argument"),
                span: (s.span.start, s.span.end),
            });
        }
        Some(other) => {
            let options_reg = compile_expr(cx, other.to_expression(), span)?;
            let cause_reg = cx.alloc_scratch();
            cx.emit_load_property(cause_reg, options_reg, "cause", span);
            Some(cause_reg)
        }
    };
    if kind == "Error" {
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::NewError,
            [Operand::Register(dst), Operand::Register(msg_reg)],
            span,
        );
        if let Some(cause_reg) = cause_reg {
            let key_idx = cx.intern_string_constant("cause");
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(dst),
                    Operand::ConstIndex(key_idx),
                    Operand::Register(cause_reg),
                    Operand::Register(scratch),
                ],
                span,
            );
        }
        return Ok(dst);
    }
    let dst = cx.alloc_scratch();
    let kind_idx = cx.intern_string_constant(kind);
    cx.emit(
        Op::NewBuiltinError,
        vec![
            Operand::Register(dst),
            Operand::ConstIndex(kind_idx),
            Operand::Register(msg_reg),
        ],
        span,
    );
    if let Some(cause_reg) = cause_reg {
        let key_idx = cx.intern_string_constant("cause");
        let scratch = cx.alloc_scratch();
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(dst),
                Operand::ConstIndex(key_idx),
                Operand::Register(cause_reg),
                Operand::Register(scratch),
            ],
            span,
        );
    }
    Ok(dst)
}
