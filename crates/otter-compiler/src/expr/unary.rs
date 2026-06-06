//! Unary and update expression lowering.
//!
//! # Contents
//! - [`compile_unary`] — lowers unary expressions.
//! - [`compile_update`] — lowers prefix and postfix update expressions.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::{UnaryExpression, UpdateExpression};

pub(crate) fn compile_unary(
    cx: &mut Compiler,
    u: &UnaryExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (u.span.start, u.span.end);
    // `delete obj.prop` is special: the operand isn't a
    // value-producing expression, it's a member reference.
    if matches!(u.operator, UnaryOperator::Delete) {
        if cx.is_strict
            && let Expression::Identifier(id) = &u.argument
        {
            return Err(CompileError::Unsupported {
                node: format!("strict delete of identifier `{}`", id.name.as_str()),
                span,
            });
        }
        if let Expression::StaticMemberExpression(member) = &u.argument {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::DeleteProperty,
                vec![
                    Operand::Register(dst),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            return Ok(dst);
        }
        if let Expression::ComputedMemberExpression(member) = &u.argument {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let idx_reg = compile_expr(cx, &member.expression, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::DeleteElement,
                vec![
                    Operand::Register(dst),
                    Operand::Register(obj_reg),
                    Operand::Register(idx_reg),
                ],
                span,
            );
            return Ok(dst);
        }
        // §13.5.1.2 — sloppy `delete Identifier` resolves the binding:
        // a `with` object environment or the global object deletes
        // the property (result reflects configurability); a
        // declarative binding yields `false`; an unresolvable
        // reference yields `true`.
        if let Expression::Identifier(id) = &u.argument {
            let name = id.name.as_str().to_string();
            let dst = cx.alloc_scratch();
            let active_with_envs = cx.active_with_envs.clone();
            let probe =
                crate::with_statement::emit_with_binding_probe(cx, &name, &active_with_envs, span)?;
            let mut with_done = None;
            if let Some(probe) = &probe {
                let fallback =
                    cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
                let name_idx = cx.intern_string_constant(&name);
                cx.emit(
                    Op::DeleteProperty,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(probe.object_reg),
                        Operand::ConstIndex(name_idx),
                    ],
                    span,
                );
                with_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
                cx.patch_branch_to_here(fallback);
            }
            if cx.lookup_binding(&name).is_some() || cx.resolve_capture(&name).is_some() {
                // §9.1.1.1.7 DeleteBinding on a declarative
                // environment record — bindings created by
                // declarations are not deletable.
                cx.emit(Op::LoadFalse, [Operand::Register(dst)], span);
            } else {
                // Global fallback: §9.1.1.4.7 deletes the global
                // object property; `DeleteProperty` already returns
                // `false` for non-configurable entries (script vars,
                // functions) and `true` for absent / configurable
                // ones.
                let global_reg = cx.alloc_scratch();
                cx.emit(Op::LoadGlobalThis, [Operand::Register(global_reg)], span);
                let name_idx = cx.intern_string_constant(&name);
                cx.emit(
                    Op::DeleteProperty,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(global_reg),
                        Operand::ConstIndex(name_idx),
                    ],
                    span,
                );
            }
            if let Some(done) = with_done {
                cx.patch_branch_to_here(done);
            }
            return Ok(dst);
        }
        // §13.5.1.2 — `delete` on a non-Reference returns
        // `true`. The argument is still evaluated for side
        // effects, then we discard it.
        // <https://tc39.es/ecma262/#sec-delete-operator-runtime-semantics-evaluation>
        let _ = compile_expr(cx, &u.argument, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadTrue, [Operand::Register(dst)], span);
        return Ok(dst);
    }
    // §13.5.2 `void expr` — evaluate, discard, return `undefined`.
    // <https://tc39.es/ecma262/#sec-void-operator>
    if matches!(u.operator, UnaryOperator::Void) {
        let _ = compile_expr(cx, &u.argument, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
        return Ok(dst);
    }
    // §13.5.3 `typeof Identifier` — IsUnresolvableReference
    // returns `"undefined"` rather than throwing
    // ReferenceError. Re-route the global-fallback step to
    // `LoadGlobalOrUndefined` so an unbound free identifier
    // never throws under `typeof`.
    // <https://tc39.es/ecma262/#sec-typeof-operator>
    if matches!(u.operator, UnaryOperator::Typeof)
        && let Expression::Identifier(id) = &u.argument
    {
        let name = id.name.as_str();
        if cx.lookup_binding(name).is_none()
            && find_module_import_binding(cx, name).is_none()
            && cx.resolve_capture(name).is_none()
            && !is_builtin_error_class_name(name)
            && name != "NaN"
            && name != "Infinity"
            && name != "undefined"
        {
            let value_reg = cx.alloc_scratch();
            // §9.1.1.2.1 — an enclosing `with` environment shadows the
            // global fallback; probe it first so `typeof name` sees
            // the with-object's property.
            let active_with_envs = cx.active_with_envs.clone();
            let probe =
                crate::with_statement::emit_with_binding_probe(cx, name, &active_with_envs, span)?;
            let mut with_done = None;
            if let Some(probe) = &probe {
                let fallback =
                    cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
                cx.emit_load_property(value_reg, probe.object_reg, name, span);
                with_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
                cx.patch_branch_to_here(fallback);
            }
            let name_idx = cx.intern_string_constant(name);
            // An eval-introduced frame binding shadows the global
            // fallback inside a function with a direct eval.
            let op = if cx.contains_direct_eval {
                Op::TypeofDynamic
            } else {
                Op::LoadGlobalOrUndefined
            };
            cx.emit(
                op,
                [Operand::Register(value_reg), Operand::ConstIndex(name_idx)],
                span,
            );
            if let Some(done) = with_done {
                cx.patch_branch_to_here(done);
            }
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::TypeOf,
                [Operand::Register(dst), Operand::Register(value_reg)],
                span,
            );
            return Ok(dst);
        }
    }
    let inner = compile_expr(cx, &u.argument, span)?;
    let dst = cx.alloc_scratch();
    let op = match u.operator {
        UnaryOperator::UnaryNegation => Op::Neg,
        UnaryOperator::UnaryPlus => Op::ToNumber,
        UnaryOperator::LogicalNot => Op::LogicalNot,
        UnaryOperator::BitwiseNot => Op::BitwiseNot,
        UnaryOperator::Typeof => Op::TypeOf,
        other => {
            return Err(CompileError::Unsupported {
                node: format!("UnaryExpression ({other:?})"),
                span,
            });
        }
    };
    // §13.5.4–13.5.7 — unary `+`, `-`, `~` apply
    // ToPrimitive(number) before ToNumeric so an object
    // operand goes through `[Symbol.toPrimitive]` /
    // `valueOf` / `toString`. LogicalNot and TypeOf do
    // not coerce; they take their argument as-is.
    // <https://tc39.es/ecma262/#sec-unary-operators>
    let inner_in = match op {
        Op::Neg | Op::ToNumber | Op::BitwiseNot => emit_to_primitive(cx, inner, "number", span),
        _ => inner,
    };
    cx.emit(
        op,
        [Operand::Register(dst), Operand::Register(inner_in)],
        span,
    );
    Ok(dst)
}

pub(crate) fn compile_update(
    cx: &mut Compiler,
    u: &UpdateExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (u.span.start, u.span.end);
    // §13.4 UpdateExpression — applies to identifiers,
    // static / computed member access, and (private-field
    // access — deferred). Refactor: compute a load + store
    // closure pair so the increment / decrement loop is
    // shared across target shapes.
    // <https://tc39.es/ecma262/#sec-update-expressions>
    let old = cx.alloc_scratch();
    // Storage closure result: a function the store path
    // reads to write `next` back to the same target.
    enum UpdateTarget<'b, 'c> {
        Identifier {
            name: String,
            storage: Option<BindingStorage>,
            with_ref: Option<WithBindingProbe>,
        },
        StaticMember {
            obj_reg: u16,
            name: &'b str,
        },
        ComputedMember {
            obj_reg: u16,
            key_reg: u16,
            _phantom: std::marker::PhantomData<&'c ()>,
        },
        SuperStatic {
            home_reg: u16,
            name: &'b str,
        },
        SuperComputed {
            home_reg: u16,
            key_reg: u16,
            _phantom: std::marker::PhantomData<&'c ()>,
        },
    }
    let target = match &u.argument {
        SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
            let name = id.name.as_str().to_string();
            let storage = match cx.lookup_binding(&name) {
                Some(info) if info.is_const => {
                    return Err(CompileError::Unsupported {
                        node: format!("update on const `{name}`"),
                        span,
                    });
                }
                Some(info) => Some(info.storage),
                None => cx
                    .resolve_capture(&name)
                    .map(|idx| BindingStorage::Upvalue { idx }),
            };
            let active_with_envs = cx.active_with_envs.clone();
            let with_ref = emit_with_binding_probe(cx, &name, &active_with_envs, span)?;
            let mut with_done = None;
            if let Some(probe) = &with_ref {
                let fallback =
                    cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
                cx.emit_load_property(old, probe.object_reg, &name, span);
                with_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
                cx.patch_branch_to_here(fallback);
            }
            match storage {
                Some(s) => cx.emit_load_storage(old, s, span),
                None => {
                    // §13.4.2 — GetValue resolves through the global
                    // environment (realm-wide lexicals first); a
                    // missing binding is a ReferenceError.
                    let name_idx = cx.intern_string_constant(&name);
                    cx.emit(
                        Op::LoadGlobalOrThrow,
                        [Operand::Register(old), Operand::ConstIndex(name_idx)],
                        span,
                    );
                }
            }
            if let Some(done) = with_done {
                cx.patch_branch_to_here(done);
            }
            UpdateTarget::Identifier {
                name,
                storage,
                with_ref,
            }
        }
        SimpleAssignmentTarget::StaticMemberExpression(member)
            if matches!(member.object, Expression::Super(_)) =>
        {
            // §13.4 update on `super.name` — read and write both go
            // through the super reference (parent-prototype lookup,
            // `this` receiver).
            let home_reg = load_synthetic_capture(cx, SUPER_HOME_NAME, span)?;
            let name = member.property.name.as_str();
            let name_idx = cx.intern_string_constant(name);
            cx.emit(
                Op::LoadSuperProperty,
                vec![
                    Operand::Register(old),
                    Operand::Register(home_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            UpdateTarget::SuperStatic { home_reg, name }
        }
        SimpleAssignmentTarget::ComputedMemberExpression(member)
            if matches!(member.object, Expression::Super(_)) =>
        {
            let home_reg = load_synthetic_capture(cx, SUPER_HOME_NAME, span)?;
            // §13.3.7.1 step 2 — `GetThisBinding` precedes key
            // evaluation (derived-constructor TDZ fires first).
            let this_guard = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
            let key_reg = compile_expr(cx, &member.expression, span)?;
            cx.emit(
                Op::LoadSuperElement,
                vec![
                    Operand::Register(old),
                    Operand::Register(home_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            UpdateTarget::SuperComputed {
                home_reg,
                key_reg,
                _phantom: std::marker::PhantomData,
            }
        }
        SimpleAssignmentTarget::StaticMemberExpression(member) => {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let name = member.property.name.as_str();
            cx.emit_load_property(old, obj_reg, name, span);
            UpdateTarget::StaticMember { obj_reg, name }
        }
        SimpleAssignmentTarget::ComputedMemberExpression(member) => {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let key_reg = compile_expr(cx, &member.expression, span)?;
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(old),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            UpdateTarget::ComputedMember {
                obj_reg,
                key_reg,
                _phantom: std::marker::PhantomData,
            }
        }
        _ => {
            return Err(CompileError::Unsupported {
                node: "UpdateExpression on non-identifier operand".to_string(),
                span,
            });
        }
    };

    // §13.4 step 3 — coerce to number (or BigInt via
    // ToNumeric). The shared run-numeric path now handles
    // primitives so `cur` can flow through Add/Sub directly.
    let cur = cx.alloc_scratch();
    cx.emit(
        Op::ToNumber,
        [Operand::Register(cur), Operand::Register(old)],
        span,
    );
    let one = cx.alloc_scratch();
    cx.emit(
        Op::LoadInt32,
        [Operand::Register(one), Operand::Imm32(1)],
        span,
    );
    let next = cx.alloc_scratch();
    let op = match u.operator {
        UpdateOperator::Increment => Op::Add,
        UpdateOperator::Decrement => Op::Sub,
    };
    cx.emit(
        op,
        vec![
            Operand::Register(next),
            Operand::Register(cur),
            Operand::Register(one),
        ],
        span,
    );
    match target {
        UpdateTarget::Identifier {
            name,
            storage,
            with_ref,
        } => {
            let mut with_store_done = None;
            if let Some(probe) = &with_ref {
                let fallback =
                    cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
                let name_idx = cx.intern_string_constant(&name);
                let scratch = cx.alloc_scratch();
                cx.emit(
                    Op::StoreProperty,
                    vec![
                        Operand::Register(probe.object_reg),
                        Operand::ConstIndex(name_idx),
                        Operand::Register(next),
                        Operand::Register(scratch),
                    ],
                    span,
                );
                with_store_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
                cx.patch_branch_to_here(fallback);
            }
            match storage {
                Some(s) => cx.emit_store_storage(next, s, span),
                None => {
                    // §9.1.1.4 global SetMutableBinding — realm-wide
                    // lexicals shadow the object record.
                    let name_idx = cx.intern_string_constant(&name);
                    let strict = i32::from(cx.is_strict);
                    cx.emit(
                        Op::StoreGlobalBinding,
                        [
                            Operand::Register(next),
                            Operand::ConstIndex(name_idx),
                            Operand::Imm32(strict),
                        ],
                        span,
                    );
                }
            }
            if let Some(done) = with_store_done {
                cx.patch_branch_to_here(done);
            }
        }
        UpdateTarget::StaticMember { obj_reg, name } => {
            let name_idx = cx.intern_string_constant(name);
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(next),
                    Operand::Register(scratch),
                ],
                span,
            );
        }
        UpdateTarget::ComputedMember {
            obj_reg, key_reg, ..
        } => {
            cx.emit_store_element(obj_reg, key_reg, next, span);
        }
        UpdateTarget::SuperStatic { home_reg, name } => {
            let name_idx = cx.intern_string_constant(name);
            cx.emit(
                Op::SetSuperProperty,
                vec![
                    Operand::Register(home_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(next),
                ],
                span,
            );
        }
        UpdateTarget::SuperComputed {
            home_reg, key_reg, ..
        } => {
            cx.emit(
                Op::SetSuperElement,
                vec![
                    Operand::Register(home_reg),
                    Operand::Register(key_reg),
                    Operand::Register(next),
                ],
                span,
            );
        }
    }
    // §13.4.2.1 / 13.4.3.1 — postfix returns the pre-
    // update value (post-ToNumber); prefix returns the
    // new value.
    Ok(if u.prefix { next } else { cur })
}
