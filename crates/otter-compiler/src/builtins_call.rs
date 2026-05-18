//! Compiler-lowered builtin call emission helpers.
//!
//! # Contents
//! - error constructor fast paths
//! - Object static method lowering
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
    // §20.5.7.1 — `new AggregateError(errors, message?)` accepts two
    // arguments. The first is the error iterable; the second is the
    // optional message. Lower as `NewBuiltinError(message)` followed
    // by `StoreProperty("errors", errors_arg)`.
    if kind == "AggregateError" {
        if arguments.len() > 2 {
            return Err(CompileError::Unsupported {
                node: format!("{kind}: more than two arguments"),
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
        return Ok(dst);
    }
    if arguments.len() > 1 {
        return Err(CompileError::Unsupported {
            node: format!("{kind}: more than one argument (foundation accepts only `message`)"),
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
    if kind == "Error" {
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::NewError,
            [Operand::Register(dst), Operand::Register(msg_reg)],
            span,
        );
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
    Ok(dst)
}

/// Lower a recognised `Object.<method>(args...)` call site to its
/// dedicated opcode. Foundation slice 19 covers `create`,
/// `getPrototypeOf`, and `setPrototypeOf`.
pub(crate) fn compile_object_builtin(
    cx: &mut Compiler,
    method: &str,
    arg_regs: &[u16],
    span: (u32, u32),
) -> Result<u16, CompileError> {
    match (method, arg_regs.len()) {
        // §20.1.2.2 `Object.create(O [, Properties])` — synthesise
        // `undefined` for missing arguments so the runtime can throw
        // the spec `TypeError("Object.create: O must be Object or
        // Null")` rather than the compiler rejecting the call shape.
        // 1-arg shape uses the fast-path `NewObject` + `SetPrototype`;
        // 2-arg shape routes through the runtime so descriptor
        // coercion stays alongside `defineProperties`. Extra
        // arguments fall through to the 2-arg path (the runtime
        // ignores trailing args).
        // <https://tc39.es/ecma262/#sec-object.create>
        ("create", n) => {
            let proto_reg = if let Some(reg) = arg_regs.first().copied() {
                reg
            } else {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                dst
            };
            if n <= 1 {
                let dst = cx.alloc_scratch();
                cx.emit(Op::NewObject, [Operand::Register(dst)], span);
                cx.emit(
                    Op::SetPrototype,
                    [Operand::Register(dst), Operand::Register(proto_reg)],
                    span,
                );
                Ok(dst)
            } else {
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::ObjectCall,
                    vec![
                        Operand::Register(dst),
                        Operand::ConstIndex(
                            otter_bytecode::method_id::ObjectMethod::Create.as_u32(),
                        ),
                        Operand::ConstIndex(2),
                        Operand::Register(proto_reg),
                        Operand::Register(arg_regs[1]),
                    ],
                    span,
                );
                Ok(dst)
            }
        }
        // §20.1.2.12 `Object.getPrototypeOf(O)` — accept any arity so
        // the runtime can surface the spec `TypeError` when `O` is
        // missing / `undefined` / `null`.
        // <https://tc39.es/ecma262/#sec-object.getprototypeof>
        ("getPrototypeOf", _) => {
            let obj_reg = if let Some(reg) = arg_regs.first().copied() {
                reg
            } else {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                dst
            };
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::GetPrototype,
                [Operand::Register(dst), Operand::Register(obj_reg)],
                span,
            );
            Ok(dst)
        }
        // §20.1.2.21 `Object.setPrototypeOf(O, proto)`. Fast-path
        // lowering on the canonical 2-arg shape; off-arity calls
        // synthesize `undefined` for the missing operand and reuse
        // the same `Op::SetPrototype` arm so the runtime can throw
        // the spec-required `TypeError` (proto must be Object/Null,
        // O must be coercible) rather than the compiler rejecting
        // the call shape outright.
        ("setPrototypeOf", _) => {
            let obj_reg = if let Some(reg) = arg_regs.first().copied() {
                reg
            } else {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                dst
            };
            let proto_reg = if let Some(reg) = arg_regs.get(1).copied() {
                reg
            } else {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                dst
            };
            cx.emit(
                Op::SetPrototype,
                [Operand::Register(obj_reg), Operand::Register(proto_reg)],
                span,
            );
            Ok(obj_reg)
        }
        // §20.1.2.13 `Object.is(value1, value2)` — lowers to
        // [`Op::SameValue`], which dispatches §7.2.11 SameValue.
        // Missing arguments default to `undefined`; extra arguments
        // are ignored per §10.4.4 (built-in call surface).
        // <https://tc39.es/ecma262/#sec-object.is>
        ("is", _) => {
            let x_reg = if let Some(reg) = arg_regs.first().copied() {
                reg
            } else {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                dst
            };
            let y_reg = if let Some(reg) = arg_regs.get(1).copied() {
                reg
            } else {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
                dst
            };
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::SameValue,
                vec![
                    Operand::Register(dst),
                    Operand::Register(x_reg),
                    Operand::Register(y_reg),
                ],
                span,
            );
            Ok(dst)
        }
        // ECMA-262 §20.1.2 / §10.1.6 — Object descriptor surface.
        // Typed dispatch via [`ObjectMethod`].
        // <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
        _ if otter_bytecode::method_id::ObjectMethod::from_str(method).is_some() => {
            let method_id = otter_bytecode::method_id::ObjectMethod::from_str(method)
                .expect("guard above ensures Some");
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.iter().copied().map(Operand::Register));
            cx.emit(Op::ObjectCall, operands, span);
            Ok(dst)
        }
        _ => Err(CompileError::Unsupported {
            node: format!("Object.{method}/{}", arg_regs.len()),
            span,
        }),
    }
}
