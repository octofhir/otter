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
        // §13.5.1 — parentheses preserve the Reference, so
        // `delete (x)` is the identifier / member form too.
        let mut delete_arg = &u.argument;
        while let Expression::ParenthesizedExpression(p) = delete_arg {
            delete_arg = &p.expression;
        }
        let delete_arg = delete_arg;
        if cx.is_strict
            && let Expression::Identifier(id) = delete_arg
        {
            return Err(CompileError::Unsupported {
                node: format!("strict delete of identifier `{}`", id.name.as_str()),
                span,
            });
        }
        // §13.5.1.2 step 5.b — `delete super.x` / `delete super[k]`
        // throws ReferenceError at runtime, after the SuperProperty
        // reference itself evaluates: GetThisBinding fires first
        // (derived-constructor TDZ, §13.3.7.1 step 2), then the
        // computed key expression (GetValue only — ToPropertyKey is
        // never reached). GetSuperBase is unobservable here, and the
        // modern spec creates the reference without coercing the
        // base, so a null prototype still yields ReferenceError.
        if let Expression::StaticMemberExpression(member) = delete_arg
            && matches!(member.object, Expression::Super(_))
        {
            let this_guard = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
            return Ok(emit_delete_super_reference_error(cx, span));
        }
        if let Expression::ComputedMemberExpression(member) = delete_arg
            && matches!(member.object, Expression::Super(_))
        {
            let this_guard = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
            let _ = compile_expr(cx, &member.expression, span)?;
            return Ok(emit_delete_super_reference_error(cx, span));
        }
        if let Expression::StaticMemberExpression(member) = delete_arg {
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
        if let Expression::ComputedMemberExpression(member) = delete_arg {
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
        if let Expression::Identifier(id) = delete_arg {
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
            } else if cx.any_enclosing_direct_eval() {
                // §19.2.1.3 — eval-created var bindings are
                // CreateMutableBinding(vn, true): deletable. The name
                // may live in the frame's eval-var map / captured
                // eval-env chain; otherwise the op falls through to
                // the global-object delete.
                let name_idx = cx.intern_string_constant(&name);
                cx.emit(
                    Op::DeleteDynamic,
                    [Operand::Register(dst), Operand::ConstIndex(name_idx)],
                    span,
                );
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
        let _ = compile_expr(cx, delete_arg, span)?;
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
    // §13.5.3 — parentheses preserve the Reference, so
    // `typeof (x)` is the identifier form too.
    let mut typeof_arg = &u.argument;
    while let Expression::ParenthesizedExpression(p) = typeof_arg {
        typeof_arg = &p.expression;
    }
    if matches!(u.operator, UnaryOperator::Typeof)
        && let Expression::Identifier(id) = typeof_arg
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
            let op = if cx.any_enclosing_direct_eval() {
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
    // The operand (and its ToPrimitive temp) is dead once the unary op
    // has read it, so recycle the range into `dst`. See
    // `FunctionContext::reset_scratch`.
    let mark = cx.scratch;
    let inner = compile_expr(cx, &u.argument, span)?;
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
    // A provably-primitive operand skips ToPrimitive (no observable
    // valueOf / [Symbol.toPrimitive]); the op's own ToNumeric still runs.
    let inner_in = match op {
        Op::Neg | Op::ToNumber | Op::BitwiseNot
            if !crate::expr::binary::expr_is_primitive(&u.argument) =>
        {
            emit_to_primitive(cx, inner, "number", span)
        }
        _ => inner,
    };
    cx.reset_scratch(mark);
    let dst = cx.alloc_scratch();
    cx.emit(
        op,
        [Operand::Register(dst), Operand::Register(inner_in)],
        span,
    );
    Ok(dst)
}

/// §13.5.1.2 step 5.b — emit the unconditional ReferenceError a
/// `delete` on a super-reference produces. Returns a result register
/// (never reached at runtime) so the caller stays expression-shaped.
fn emit_delete_super_reference_error(cx: &mut Compiler, span: (u32, u32)) -> u16 {
    let message_reg = cx.alloc_scratch();
    let message = cx.intern_string_constant("Cannot delete a super property");
    cx.emit(
        Op::LoadString,
        [Operand::Register(message_reg), Operand::ConstIndex(message)],
        span,
    );
    let error_reg = cx.alloc_scratch();
    let kind = cx.intern_string_constant("ReferenceError");
    cx.emit(
        Op::NewBuiltinError,
        [
            Operand::Register(error_reg),
            Operand::ConstIndex(kind),
            Operand::Register(message_reg),
        ],
        span,
    );
    cx.emit(Op::Throw, [Operand::Register(error_reg)], span);
    let result = cx.alloc_scratch();
    cx.emit(Op::LoadUndefined, [Operand::Register(result)], span);
    result
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
        PrivateField {
            obj_reg: u16,
            key_reg: u16,
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
            if let Some(info) = cx.lookup_binding(&name).filter(|info| info.is_const) {
                cx.emit_load_storage(old, info.storage, span);
                return finish_const_update(cx, &name, old, u, span);
            }
            let storage = match cx.lookup_binding(&name) {
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
                // §9.1.1.2.6 — GetBindingValue re-checks HasProperty
                // before the Get (the probe's getter may have deleted
                // the binding).
                crate::with_statement::emit_with_get_binding_value(
                    cx,
                    old,
                    probe.object_reg,
                    &name,
                    span,
                );
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
        SimpleAssignmentTarget::PrivateFieldExpression(member) => {
            // §13.4 update on `obj.#field`. The reference is evaluated
            // once: the same object and private-key registers back both
            // the read (`PrivateGet`) and the write (`PrivateSet`), so a
            // side-effecting object expression runs a single time.
            let obj_reg = compile_expr(cx, &member.object, span)?;
            crate::class::emit_private_method_brand_check(
                cx,
                obj_reg,
                member.field.name.as_str(),
                span,
            )?;
            let key_reg = crate::class::load_private_key(cx, member.field.name.as_str(), span)?;
            cx.emit(
                Op::PrivateGet,
                vec![
                    Operand::Register(old),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            UpdateTarget::PrivateField { obj_reg, key_reg }
        }
        _ => {
            return Err(CompileError::Unsupported {
                node: "UpdateExpression on non-identifier operand".to_string(),
                span,
            });
        }
    };

    // §13.4 step 3 — ToNumeric applies ToPrimitive(number) first,
    // so an object operand fires `[Symbol.toPrimitive]` / `valueOf`
    // / `toString` before the numeric coercion.
    let old_prim = emit_to_primitive(cx, old, "number", span);
    // §13.4.2 — ToNumeric preserves BigInt operands; the VM applies
    // the ±1 in the operand's own numeric type.
    let cur = cx.alloc_scratch();
    cx.emit(
        Op::ToNumeric,
        [Operand::Register(cur), Operand::Register(old_prim)],
        span,
    );
    let delta = match u.operator {
        UpdateOperator::Increment => 1,
        UpdateOperator::Decrement => -1,
    };
    let next = cx.alloc_scratch();
    cx.emit(
        Op::Increment,
        vec![
            Operand::Register(next),
            Operand::Register(cur),
            Operand::Imm32(delta),
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
                // §9.1.1.2.5 — SetMutableBinding re-checks HasProperty
                // before the Set; a binding deleted between the read
                // and the write throws ReferenceError in strict code.
                crate::with_statement::emit_with_set_mutable_binding(
                    cx,
                    probe.object_reg,
                    &name,
                    next,
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
        UpdateTarget::PrivateField { obj_reg, key_reg } => {
            cx.emit(
                Op::PrivateSet,
                vec![
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                    Operand::Register(next),
                ],
                span,
            );
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

/// §13.4.2-5 — update on a `const` binding: the old value still loads
/// and coerces through ToNumeric (firing user `valueOf` /
/// `[Symbol.toPrimitive]`), then PutValue throws TypeError at runtime.
fn finish_const_update(
    cx: &mut Compiler,
    name: &str,
    old: u16,
    u: &UpdateExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = u;
    let old_prim = emit_to_primitive(cx, old, "number", span);
    let cur = cx.alloc_scratch();
    cx.emit(
        Op::ToNumeric,
        [Operand::Register(cur), Operand::Register(old_prim)],
        span,
    );
    Ok(crate::assignment::emit_assignment_type_error(
        cx,
        &format!("Assignment to constant variable '{name}'."),
        span,
    ))
}
