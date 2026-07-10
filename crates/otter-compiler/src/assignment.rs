//! Assignment target lowering for identifiers, members, and patterns.
//!
//! # Contents
//! - plain and compound assignment
//! - logical assignment
//! - array and object pattern assignment
//! - strict assignment-target validation
//!
//! # Invariants
//! - Stores go through the same binding paths as declarations.
//!
//! # See also
//! - `destructuring` and `expr`

use crate::*;

enum PreparedAssignmentTarget {
    Identifier(String),
    StaticMember { obj_reg: u16, name_idx: u32 },
    ComputedMember { obj_reg: u16, key_reg: u16 },
    PrivateField { obj_reg: u16, key_reg: u16 },
    // `super.X` / `super[K]` targets: the home object and (for computed)
    // the key are evaluated during the prepare phase so destructuring
    // order-of-operations matches §13.15.5; the store runs through the
    // receiver via `SetSuperProperty` / `SetSuperElement`.
    SuperProperty { home_reg: u16, name_idx: u32 },
    SuperElement { home_reg: u16, idx_reg: u16 },
}

/// `Op::TdzError`'s operand carries the binding's slot index purely for
/// the runtime diagnostic message — it mirrors the read-side TDZ lowering
/// in `expr::identifier`.
fn tdz_diag_index(storage: Option<BindingStorage>) -> i32 {
    match storage {
        Some(BindingStorage::Register { reg }) => reg as i32,
        Some(BindingStorage::Upvalue { idx }) => idx as i32,
        None => 0,
    }
}

pub(crate) fn compile_assignment(
    cx: &mut Compiler,
    a: &oxc_ast::ast::AssignmentExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (a.span.start, a.span.end);
    let compound_op = compound_assign_op(a.operator);
    if a.operator.is_logical() {
        // §13.15.4 LogicalAssignment — `x &&= y`, `x ||= y`, `x ??= y`.
        // Lowered to the desugared form:
        //   `x &&= y` → if (cur) x = y;
        //   `x ||= y` → if (!cur) x = y;
        //   `x ??= y` → if (cur is null/undefined) x = y;
        // Foundation handles the identifier-target fast path here;
        // member / computed-member targets fall through to the
        // existing assignment branches via a synthesised plain-`=`.
        return compile_logical_assignment(cx, a, span);
    }
    if let AssignmentTarget::StaticMemberExpression(member) = &a.left {
        // §13.3.5.3 MakeSuperPropertyReference + §6.2.4.5 PutValue
        // step 6.b — `super.X = V` writes through the receiver
        // (`this`), not the parent prototype, so the foundation
        // lowers the store as a plain `this.X = V` write. Reads
        // still walk the parent chain via `compile_super_member_load`.
        if matches!(member.object, Expression::Super(_)) {
            // §13.3.5.3 MakeSuperPropertyReference + §6.2.5.5 PutValue
            // step 6.b — `super.X = V` resolves any accessor setter on
            // the parent prototype and invokes it with `this` as the
            // receiver; absent a setter it writes an own property onto
            // `this`. `Op::SetSuperProperty` performs both.
            let home_reg = load_synthetic_capture(cx, super_home_binding_name(cx), span)?;
            // §13.3.7.1 step 2 — `GetThisBinding` precedes RHS
            // evaluation so a derived-constructor TDZ ReferenceError
            // fires first.
            let this_guard = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let new_value = match compound_op {
                None => compile_expr(cx, &a.right, span)?,
                Some(op) => {
                    let current =
                        compile_super_member_load(cx, member.property.name.as_str(), span)?;
                    let rhs = compile_expr(cx, &a.right, span)?;
                    let (cur_p, rhs_p) = coerce_compound_operands(cx, op, current, rhs, span);
                    let dst = cx.alloc_scratch();
                    cx.emit(
                        op,
                        vec![
                            Operand::Register(dst),
                            Operand::Register(cur_p),
                            Operand::Register(rhs_p),
                        ],
                        span,
                    );
                    dst
                }
            };
            cx.emit(
                Op::SetSuperProperty,
                vec![
                    Operand::Register(home_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(new_value),
                ],
                span,
            );
            return Ok(new_value);
        }
        let obj_reg = compile_expr(cx, &member.object, span)?;
        let name_idx = cx.intern_string_constant(member.property.name.as_str());
        let new_value = match compound_op {
            None => compile_expr(cx, &a.right, span)?,
            Some(op) => {
                let current = cx.alloc_scratch();
                cx.emit(
                    Op::LoadProperty,
                    vec![
                        Operand::Register(current),
                        Operand::Register(obj_reg),
                        Operand::ConstIndex(name_idx),
                    ],
                    span,
                );
                let rhs = compile_expr(cx, &a.right, span)?;
                let (cur_p, rhs_p) = coerce_compound_operands(cx, op, current, rhs, span);
                let dst = cx.alloc_scratch();
                cx.emit(
                    op,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(cur_p),
                        Operand::Register(rhs_p),
                    ],
                    span,
                );
                dst
            }
        };
        let store_scratch = cx.alloc_scratch();
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_idx),
                Operand::Register(new_value),
                Operand::Register(store_scratch),
            ],
            span,
        );
        return Ok(new_value);
    }
    if let AssignmentTarget::PrivateFieldExpression(member) = &a.left {
        let obj_reg = compile_expr(cx, &member.object, span)?;
        crate::class::emit_private_method_brand_check(
            cx,
            obj_reg,
            member.field.name.as_str(),
            span,
        )?;
        let key_reg = crate::class::load_private_key(cx, member.field.name.as_str(), span)?;
        let new_value = match compound_op {
            None => compile_expr(cx, &a.right, span)?,
            Some(op) => {
                let current = cx.alloc_scratch();
                cx.emit(
                    Op::PrivateGet,
                    vec![
                        Operand::Register(current),
                        Operand::Register(obj_reg),
                        Operand::Register(key_reg),
                    ],
                    span,
                );
                let rhs = compile_expr(cx, &a.right, span)?;
                let (cur_p, rhs_p) = coerce_compound_operands(cx, op, current, rhs, span);
                let dst = cx.alloc_scratch();
                cx.emit(
                    op,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(cur_p),
                        Operand::Register(rhs_p),
                    ],
                    span,
                );
                dst
            }
        };
        cx.emit(
            Op::PrivateSet,
            vec![
                Operand::Register(obj_reg),
                Operand::Register(key_reg),
                Operand::Register(new_value),
            ],
            span,
        );
        return Ok(new_value);
    }
    if let AssignmentTarget::ComputedMemberExpression(member) = &a.left {
        // `super[idx] = V` shares the receiver-targeted store with
        // its dotted counterpart per §13.3.5.3 + §6.2.4.5 step 6.b.
        if matches!(member.object, Expression::Super(_)) {
            // §13.3.5.3 + §6.2.5.5 PutValue step 6.b — `super[K] = V`
            // resolves an accessor setter on the parent prototype and
            // invokes it with `this`; absent a setter it writes an own
            // property onto `this`. `Op::SetSuperElement` performs both
            // and runs `GetSuperBase` before the key's `ToPropertyKey`.
            let home_reg = load_synthetic_capture(cx, super_home_binding_name(cx), span)?;
            // §13.3.7.1 step 2 — `GetThisBinding` precedes evaluation
            // of the key expression, so a derived-constructor TDZ
            // ReferenceError fires before any key/RHS side effects.
            let this_guard = cx.alloc_scratch();
            cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
            let idx_reg = compile_expr(cx, &member.expression, span)?;
            let new_value = match compound_op {
                None => compile_expr(cx, &a.right, span)?,
                Some(op) => {
                    let current = cx.alloc_scratch();
                    cx.emit(
                        Op::LoadSuperElement,
                        vec![
                            Operand::Register(current),
                            Operand::Register(home_reg),
                            Operand::Register(idx_reg),
                        ],
                        span,
                    );
                    let rhs = compile_expr(cx, &a.right, span)?;
                    let (cur_p, rhs_p) = coerce_compound_operands(cx, op, current, rhs, span);
                    let dst = cx.alloc_scratch();
                    cx.emit(
                        op,
                        vec![
                            Operand::Register(dst),
                            Operand::Register(cur_p),
                            Operand::Register(rhs_p),
                        ],
                        span,
                    );
                    dst
                }
            };
            cx.emit(
                Op::SetSuperElement,
                vec![
                    Operand::Register(home_reg),
                    Operand::Register(idx_reg),
                    Operand::Register(new_value),
                ],
                span,
            );
            return Ok(new_value);
        }
        let arr_reg = compile_expr(cx, &member.object, span)?;
        let idx_reg = compile_expr(cx, &member.expression, span)?;
        let new_value = match compound_op {
            None => compile_expr(cx, &a.right, span)?,
            Some(op) => {
                let current = cx.alloc_scratch();
                cx.emit(
                    Op::LoadElement,
                    vec![
                        Operand::Register(current),
                        Operand::Register(arr_reg),
                        Operand::Register(idx_reg),
                    ],
                    span,
                );
                let rhs = compile_expr(cx, &a.right, span)?;
                let (cur_p, rhs_p) = coerce_compound_operands(cx, op, current, rhs, span);
                let dst = cx.alloc_scratch();
                cx.emit(
                    op,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(cur_p),
                        Operand::Register(rhs_p),
                    ],
                    span,
                );
                dst
            }
        };
        cx.emit_store_element(arr_reg, idx_reg, new_value, span);
        return Ok(new_value);
    }
    // §13.15.1 Static Semantics: Early Errors — destructuring
    // AssignmentTarget identifiers cannot bind `eval` / `arguments`
    // or any strict-mode-reserved word (the `IdentifierReference`
    // grammar excludes them, and §13.1.1 makes the early error
    // explicit). Walk the LHS before lowering so the runner sees a
    // parse-phase SyntaxError instead of running the destructuring.
    if cx.is_strict
        && matches!(
            &a.left,
            AssignmentTarget::ArrayAssignmentTarget(_)
                | AssignmentTarget::ObjectAssignmentTarget(_)
        )
    {
        validate_strict_assignment_target(&a.left)?;
    }
    // §13.15.5 DestructuringAssignmentEvaluation — array / object
    // destructuring assignment targets recurse through the helper.
    if let AssignmentTarget::ArrayAssignmentTarget(arr) = &a.left {
        let value_reg = compile_expr(cx, &a.right, span)?;
        assign_array_pattern(cx, arr, value_reg, span)?;
        return Ok(value_reg);
    }
    if let AssignmentTarget::ObjectAssignmentTarget(o) = &a.left {
        let value_reg = compile_expr(cx, &a.right, span)?;
        assign_object_pattern(cx, o, value_reg, span)?;
        return Ok(value_reg);
    }
    // `name = value` — local or captured-upvalue store.
    let (name, direct_identifier) = match &a.left {
        AssignmentTarget::AssignmentTargetIdentifier(id) => (
            id.name.as_str().to_string(),
            // §13.15.2 NamedEvaluation applies only to a DIRECT
            // IdentifierReference target — a parenthesized cover
            // (`(x) = fn`) starts before the identifier's own span
            // and must not name the function.
            id.span.start == a.span.start,
        ),
        _ => {
            return Err(CompileError::Unsupported {
                node: "AssignmentTarget (non-identifier)".to_string(),
                span,
            });
        }
    };
    // §16.2.1.7 CreateImportBinding — import bindings are immutable
    // indirect bindings. Module code is strict, so assigning to one
    // throws TypeError at runtime (after evaluating the RHS for its
    // side effects), not a compile error.
    if crate::module_state::find_module_import_binding(cx, &name).is_some() {
        let _ = compile_expr(cx, &a.right, span)?;
        return Ok(emit_assignment_type_error(
            cx,
            &format!("Assignment to constant variable '{name}'."),
            span,
        ));
    }
    // §6.2.4.6 PutValue → §9.1.1.1.5 SetMutableBinding: assigning to a
    // `let` binding before its declaration's initializer ran is a TDZ
    // access. The lexical pre-pass declares the name (uninitialized) at
    // block entry, so the miss is observable statically.
    let binding_uninitialized = matches!(
        cx.lookup_binding(&name),
        Some(info) if !info.is_const && !info.initialized
    );
    // A cross-function capture has no local binding, so its TDZ cannot be
    // settled statically: the assignment store must runtime-check the
    // upvalue cell for the hole. Same-function stores never need this —
    // the static `binding_uninitialized` path covers their TDZ.
    let mut capture_store = false;
    // §10.2.11 — a named function expression's self-name binding is
    // immutable: the RHS still evaluates, then strict mode throws
    // TypeError while sloppy mode silently drops the write. Covers
    // both the local binding and a capture from an enclosing
    // function-expression body (arrows).
    let fn_self_target = match cx.lookup_binding(&name) {
        Some(info) => info.fn_self_name,
        None => cx.stack.iter().rev().skip(1).any(|frame| {
            frame
                .scopes
                .iter()
                .rev()
                .find_map(|scope| scope.bindings.get(&name))
                .is_some_and(|info| info.fn_self_name)
        }),
    };
    if fn_self_target && compound_op.is_none() {
        let value = crate::expr::compile_expr_with_inferred_name(cx, &a.right, &name, span)?;
        // §6.2.5.5 PutValue — a class-name binding assigned inside
        // its own heritage expression is still in the TDZ, which
        // outranks the immutable-binding TypeError.
        if binding_uninitialized {
            let diag_idx = tdz_diag_index(cx.lookup_binding(&name).map(|info| info.storage));
            cx.emit(Op::TdzError, [Operand::Imm32(diag_idx)], span);
            return Ok(value);
        }
        if cx.is_strict {
            return Ok(emit_assignment_type_error(
                cx,
                &format!("Assignment to constant variable '{name}'."),
                span,
            ));
        }
        return Ok(value);
    }
    if fn_self_target {
        if binding_uninitialized {
            // Compound assignment reads the target first — the TDZ
            // ReferenceError fires before the RHS evaluates.
            let diag_idx = tdz_diag_index(cx.lookup_binding(&name).map(|info| info.storage));
            cx.emit(Op::TdzError, [Operand::Imm32(diag_idx)], span);
        }
        // Compound assignment reads the current value, evaluates the
        // RHS, then hits the same immutable-store rule.
        let storage = cx
            .lookup_binding(&name)
            .map(|info| info.storage)
            .or_else(|| {
                cx.resolve_capture(&name)
                    .map(|idx| BindingStorage::Upvalue { idx })
            });
        let current = cx.alloc_scratch();
        if let Some(s) = storage {
            cx.emit_load_storage(current, s, span);
        }
        let rhs = compile_expr(cx, &a.right, span)?;
        let op = compound_op.expect("guarded");
        let (cur_p, rhs_p) = coerce_compound_operands(cx, op, current, rhs, span);
        let dst = cx.alloc_scratch();
        cx.emit(
            op,
            vec![
                Operand::Register(dst),
                Operand::Register(cur_p),
                Operand::Register(rhs_p),
            ],
            span,
        );
        if cx.is_strict {
            return Ok(emit_assignment_type_error(
                cx,
                &format!("Assignment to constant variable '{name}'."),
                span,
            ));
        }
        return Ok(dst);
    }
    let storage = match cx.lookup_binding(&name) {
        Some(info) if info.is_const => {
            // §13.15.2 PutValue on an immutable binding is a runtime
            // TypeError, not an early error: the RHS still evaluates
            // for its side effects before the throw.
            let _ = compile_expr(cx, &a.right, span)?;
            return Ok(emit_assignment_type_error(
                cx,
                &format!("Assignment to constant variable '{name}'."),
                span,
            ));
        }
        Some(info) => Some(info.storage),
        // §10.2.4.1 PutValue fallback — assignment to an undeclared
        // identifier in sloppy mode creates a property on the
        // global object. Foundation lowers this as a `StoreProperty`
        // against `globalThis` so harness-style code that pre-
        // populates globals (e.g. `assert.sameValue = function …`
        // before the first reference) keeps working.
        // <https://tc39.es/ecma262/#sec-putvalue>
        None => {
            // A `const` captured from an enclosing function is not in
            // this frame's scopes, so `lookup_binding` misses it. §13.15.2
            // PutValue on an immutable binding throws TypeError at
            // runtime after the RHS is evaluated for its side effects.
            let captured_const = cx.stack.iter().rev().skip(1).any(|frame| {
                frame
                    .scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.bindings.get(&name))
                    .is_some_and(|info| info.is_const)
            });
            if captured_const {
                let _ = compile_expr(cx, &a.right, span)?;
                return Ok(emit_assignment_type_error(
                    cx,
                    &format!("Assignment to constant variable '{name}'."),
                    span,
                ));
            }
            let captured = cx
                .resolve_capture(&name)
                .map(|idx| BindingStorage::Upvalue { idx });
            capture_store = captured.is_some();
            captured
        }
    };
    let active_with_envs = cx.active_with_envs.clone();
    let with_ref = emit_with_binding_probe(cx, &name, &active_with_envs, span)?;
    // §6.2.5.6 — a strict assignment to an unresolvable identifier
    // throws off the reference resolved BEFORE the RHS runs: snapshot
    // the global binding's existence now so a RHS side effect that
    // creates the property cannot legitimize the store.
    let strict_global_probe =
        if storage.is_none() && cx.is_strict && with_ref.is_none() && compound_op.is_none() {
            let exists = cx.alloc_scratch();
            let name_idx = cx.intern_string_constant(&name);
            cx.emit(
                Op::GlobalBindingExists,
                [Operand::Register(exists), Operand::ConstIndex(name_idx)],
                span,
            );
            Some(exists)
        } else {
            None
        };
    let value = match compound_op {
        // §13.15.2 — plain `IdentifierRef = AnonymousFunctionDefinition`
        // performs NamedEvaluation, inferring the target's name.
        None if direct_identifier => {
            crate::expr::compile_expr_with_inferred_name(cx, &a.right, &name, span)?
        }
        None => compile_expr(cx, &a.right, span)?,
        Some(op) => {
            let current = cx.alloc_scratch();
            let mut with_done = None;
            if let Some(probe) = &with_ref {
                let fallback =
                    cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
                crate::with_statement::emit_with_get_binding_value(
                    cx,
                    current,
                    probe.object_reg,
                    &name,
                    span,
                );
                with_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
                cx.patch_branch_to_here(fallback);
            }
            match storage {
                // §13.15.2 compound assignment reads the target via
                // GetValue first, so a TDZ access throws here — before
                // the RHS is evaluated for its side effects.
                Some(_) if binding_uninitialized => {
                    let diag_idx = tdz_diag_index(storage);
                    cx.emit(Op::TdzError, [Operand::Imm32(diag_idx)], span);
                }
                Some(s) => cx.emit_load_storage(current, s, span),
                None => {
                    let name_idx = cx.intern_string_constant(&name);
                    cx.emit(
                        Op::LoadGlobalOrThrow,
                        [Operand::Register(current), Operand::ConstIndex(name_idx)],
                        span,
                    );
                }
            }
            if let Some(done) = with_done {
                cx.patch_branch_to_here(done);
            }
            let rhs = compile_expr(cx, &a.right, span)?;
            let (cur_p, rhs_p) = coerce_compound_operands(cx, op, current, rhs, span);
            let dst = cx.alloc_scratch();
            cx.emit(
                op,
                vec![
                    Operand::Register(dst),
                    Operand::Register(cur_p),
                    Operand::Register(rhs_p),
                ],
                span,
            );
            dst
        }
    };
    let mut with_store_done = None;
    if let Some(probe) = &with_ref {
        let fallback = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(probe.found_reg), span);
        crate::with_statement::emit_with_set_mutable_binding(
            cx,
            probe.object_reg,
            &name,
            value,
            span,
        );
        with_store_done = Some(cx.emit_branch_placeholder(Op::Jump, None, span));
        cx.patch_branch_to_here(fallback);
    }
    match storage {
        // §6.2.4.6 PutValue on a `let` binding still in its TDZ throws
        // ReferenceError — after the RHS evaluated for its side effects.
        Some(_) if binding_uninitialized => {
            let diag_idx = tdz_diag_index(storage);
            cx.emit(Op::TdzError, [Operand::Imm32(diag_idx)], span);
        }
        Some(s) => {
            if capture_store {
                cx.emit_assign_storage(value, s, span);
            } else {
                cx.emit_store_storage(value, s, span);
            }
            cx.mark_initialized(&name);
            cx.emit_module_export_mirror(&name, value, span);
        }
        None => {
            // §9.1.1.4 global SetMutableBinding — declarative record
            // (script lexicals) first, then the object record;
            // strict stores re-check the binding still exists.
            let name_idx = cx.intern_string_constant(&name);
            if let Some(exists) = strict_global_probe {
                cx.emit(
                    Op::StoreGlobalChecked,
                    [
                        Operand::Register(value),
                        Operand::ConstIndex(name_idx),
                        Operand::Register(exists),
                    ],
                    span,
                );
            } else {
                let strict = i32::from(cx.is_strict);
                cx.emit(
                    Op::StoreGlobalBinding,
                    [
                        Operand::Register(value),
                        Operand::ConstIndex(name_idx),
                        Operand::Imm32(strict),
                    ],
                    span,
                );
            }
        }
    }
    if let Some(done) = with_store_done {
        cx.patch_branch_to_here(done);
    }
    Ok(value)
}

/// §13.15.4 LogicalAssignment — `x &&= y`, `x ||= y`, `x ??= y`.
pub(crate) fn compile_logical_assignment(
    cx: &mut Compiler,
    a: &oxc_ast::ast::AssignmentExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    use oxc_ast::ast::AssignmentOperator;
    // §13.15.2 — the target Reference evaluates ONCE: member bases
    // and computed keys are captured here and reused by the store,
    // so `base[key()] ||= rhs` never re-runs `key()`.
    enum LogicalTarget {
        Ident(String),
        Prepared(PreparedAssignmentTarget),
    }
    let (target, cur) = match &a.left {
        AssignmentTarget::AssignmentTargetIdentifier(id) => {
            let name = id.name.as_str().to_string();
            let load = cx.alloc_scratch();
            if let Some(info) = cx.lookup_binding(&name) {
                // §13.15.2 step 2 — `GetValue(lref)` runs before the
                // short-circuit test, so reading a `let`/`const`/`class`
                // binding still in its TDZ raises ReferenceError.
                if info.initialized {
                    cx.emit_load_storage(load, info.storage, span);
                } else {
                    let diag_idx = match info.storage {
                        BindingStorage::Register { reg } => reg,
                        BindingStorage::Upvalue { idx } => idx,
                    };
                    cx.emit(Op::TdzError, [Operand::Imm32(diag_idx as i32)], span);
                }
            } else if let Some(idx) = cx.resolve_capture(&name) {
                cx.emit_load_storage(load, BindingStorage::Upvalue { idx }, span);
            } else if cx.is_strict {
                let name_idx = cx.intern_string_constant(&name);
                cx.emit(
                    Op::LoadGlobalOrThrow,
                    [Operand::Register(load), Operand::ConstIndex(name_idx)],
                    span,
                );
            } else {
                let name_idx = cx.intern_string_constant(&name);
                cx.emit(
                    Op::LoadGlobalOrUndefined,
                    [Operand::Register(load), Operand::ConstIndex(name_idx)],
                    span,
                );
            }
            (LogicalTarget::Ident(name), load)
        }
        AssignmentTarget::StaticMemberExpression(m) => {
            let obj_reg = compile_expr(cx, &m.object, span)?;
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            let load = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(load),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            (
                LogicalTarget::Prepared(PreparedAssignmentTarget::StaticMember {
                    obj_reg,
                    name_idx,
                }),
                load,
            )
        }
        AssignmentTarget::ComputedMemberExpression(m) => {
            let obj_reg = compile_expr(cx, &m.object, span)?;
            let key_reg = compile_expr(cx, &m.expression, span)?;
            let load = cx.alloc_scratch();
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(load),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            (
                LogicalTarget::Prepared(PreparedAssignmentTarget::ComputedMember {
                    obj_reg,
                    key_reg,
                }),
                load,
            )
        }
        AssignmentTarget::PrivateFieldExpression(m) => {
            let obj_reg = compile_expr(cx, &m.object, span)?;
            crate::class::emit_private_method_brand_check(
                cx,
                obj_reg,
                m.field.name.as_str(),
                span,
            )?;
            let key_reg = crate::class::load_private_key(cx, m.field.name.as_str(), span)?;
            let load = cx.alloc_scratch();
            cx.emit(
                Op::PrivateGet,
                vec![
                    Operand::Register(load),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            (
                LogicalTarget::Prepared(PreparedAssignmentTarget::PrivateField {
                    obj_reg,
                    key_reg,
                }),
                load,
            )
        }
        other => {
            return Err(CompileError::Unsupported {
                node: format!("LogicalAssignment target ({other:?})"),
                span,
            });
        }
    };
    // Compute the test condition. For `&&=`, jump-if-false skips
    // the assignment. For `||=`, the `!` inverts so we use
    // jump-if-true. For `??=`, test "cur is null or undefined".
    let test_reg = match a.operator {
        AssignmentOperator::LogicalAnd => {
            let bool_r = cx.alloc_scratch();
            cx.emit(
                Op::ToBoolean,
                [Operand::Register(bool_r), Operand::Register(cur)],
                span,
            );
            bool_r
        }
        AssignmentOperator::LogicalOr => {
            let bool_r = cx.alloc_scratch();
            cx.emit(
                Op::ToBoolean,
                [Operand::Register(bool_r), Operand::Register(cur)],
                span,
            );
            let not_r = cx.alloc_scratch();
            cx.emit(
                Op::LogicalNot,
                [Operand::Register(not_r), Operand::Register(bool_r)],
                span,
            );
            not_r
        }
        AssignmentOperator::LogicalNullish => {
            let undef_r = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(undef_r)], span);
            let null_r = cx.alloc_scratch();
            cx.emit(Op::LoadNull, [Operand::Register(null_r)], span);
            let eq_undef = cx.alloc_scratch();
            cx.emit(
                Op::Equal,
                vec![
                    Operand::Register(eq_undef),
                    Operand::Register(cur),
                    Operand::Register(undef_r),
                ],
                span,
            );
            let eq_null = cx.alloc_scratch();
            cx.emit(
                Op::Equal,
                vec![
                    Operand::Register(eq_null),
                    Operand::Register(cur),
                    Operand::Register(null_r),
                ],
                span,
            );
            let merged = cx.alloc_scratch();
            cx.emit(
                Op::ToBoolean,
                [Operand::Register(merged), Operand::Register(eq_undef)],
                span,
            );
            let jump_if_true = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(merged), span);
            cx.emit(
                Op::StoreLocal,
                [Operand::Register(eq_null), Operand::Imm32(merged as i32)],
                span,
            );
            cx.patch_branch_to_here(jump_if_true);
            merged
        }
        _ => unreachable!("non-logical operator in compile_logical_assignment"),
    };
    let result = cx.alloc_scratch();
    cx.emit(
        Op::StoreLocal,
        [Operand::Register(cur), Operand::Imm32(result as i32)],
        span,
    );
    let skip = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(test_reg), span);
    // §13.15.2 step 4/6 — `IdentifierRef op= AnonymousFunction`
    // performs NamedEvaluation; member targets store through the
    // ALREADY-EVALUATED Reference.
    let new_value = match &target {
        LogicalTarget::Ident(name) => {
            crate::expr::compile_expr_with_inferred_name(cx, &a.right, name, span)?
        }
        LogicalTarget::Prepared(_) => compile_expr(cx, &a.right, span)?,
    };
    match target {
        LogicalTarget::Ident(name) => {
            // §13.15.2 PutValue on a function-expression self-name
            // binding is immutable: the RHS has already evaluated, then
            // strict mode throws while sloppy mode silently drops the
            // write. (TDZ was handled by the initial read above; const
            // bindings are rejected inside `assign_to_target`.)
            let fn_self_target = match cx.lookup_binding(&name) {
                Some(info) => info.fn_self_name,
                None => cx.stack.iter().rev().skip(1).any(|frame| {
                    frame
                        .scopes
                        .iter()
                        .rev()
                        .find_map(|scope| scope.bindings.get(&name))
                        .is_some_and(|info| info.fn_self_name)
                }),
            };
            if fn_self_target {
                if cx.is_strict {
                    emit_assignment_type_error(
                        cx,
                        &format!("Assignment to constant variable '{name}'."),
                        span,
                    );
                }
            } else {
                assign_to_target(cx, &a.left, new_value, span)?;
            }
        }
        LogicalTarget::Prepared(prepared) => {
            assign_prepared_target(cx, prepared, new_value, span)?;
        }
    }
    cx.emit(
        Op::StoreLocal,
        [Operand::Register(new_value), Operand::Imm32(result as i32)],
        span,
    );
    cx.patch_branch_to_here(skip);
    Ok(result)
}

/// §13.15.5 DestructuringAssignmentEvaluation — recurse into a
/// destructuring assignment target and store the relevant slices
/// of `value_reg` into each leaf.
///
/// Foundation handles the common shapes used across the test262
/// corpus: simple identifier leaves, nested array / object
/// destructuring, defaults via `=`, and rest elements (collected
/// via `Op::CollectRest`). Computed property keys recurse through
/// the runtime.
pub(crate) fn assign_to_target(
    cx: &mut Compiler,
    target: &oxc_ast::ast::AssignmentTarget<'_>,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    use oxc_ast::ast::AssignmentTarget;
    match target {
        AssignmentTarget::ArrayAssignmentTarget(arr) => {
            assign_array_pattern(cx, arr, value_reg, span)
        }
        AssignmentTarget::ObjectAssignmentTarget(obj) => {
            assign_object_pattern(cx, obj, value_reg, span)
        }
        AssignmentTarget::AssignmentTargetIdentifier(id) => {
            let name = id.name.as_str().to_string();
            store_identifier(cx, &name, value_reg, span)
        }
        AssignmentTarget::StaticMemberExpression(member) => {
            // `super.X` as a destructuring / for-head target writes
            // through the receiver per §13.3.5.3 + §6.2.5.5 step 6.b,
            // identical to the plain `super.X = V` lowering above.
            if matches!(member.object, Expression::Super(_)) {
                let home_reg = load_synthetic_capture(cx, super_home_binding_name(cx), span)?;
                let this_guard = cx.alloc_scratch();
                cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
                let name_idx = cx.intern_string_constant(member.property.name.as_str());
                cx.emit(
                    Op::SetSuperProperty,
                    vec![
                        Operand::Register(home_reg),
                        Operand::ConstIndex(name_idx),
                        Operand::Register(value_reg),
                    ],
                    span,
                );
                return Ok(());
            }
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(value_reg),
                    Operand::Register(scratch),
                ],
                span,
            );
            Ok(())
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            // `super[K]` target — receiver-targeted store via
            // `SetSuperElement`, mirroring the `super[K] = V` path.
            if matches!(member.object, Expression::Super(_)) {
                let home_reg = load_synthetic_capture(cx, super_home_binding_name(cx), span)?;
                let this_guard = cx.alloc_scratch();
                cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
                let idx_reg = compile_expr(cx, &member.expression, span)?;
                cx.emit(
                    Op::SetSuperElement,
                    vec![
                        Operand::Register(home_reg),
                        Operand::Register(idx_reg),
                        Operand::Register(value_reg),
                    ],
                    span,
                );
                return Ok(());
            }
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let key_reg = compile_expr(cx, &member.expression, span)?;
            cx.emit_store_element(obj_reg, key_reg, value_reg, span);
            Ok(())
        }
        AssignmentTarget::PrivateFieldExpression(member) => {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            crate::class::emit_private_method_brand_check(
                cx,
                obj_reg,
                member.field.name.as_str(),
                span,
            )?;
            let key_reg = crate::class::load_private_key(cx, member.field.name.as_str(), span)?;
            cx.emit(
                Op::PrivateSet,
                vec![
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                    Operand::Register(value_reg),
                ],
                span,
            );
            Ok(())
        }
        other => Err(CompileError::Unsupported {
            node: format!("AssignmentTarget ({other:?})"),
            span,
        }),
    }
}

fn prepare_assignment_target(
    cx: &mut Compiler,
    target: &oxc_ast::ast::AssignmentTarget<'_>,
    span: (u32, u32),
) -> Result<Option<PreparedAssignmentTarget>, CompileError> {
    use oxc_ast::ast::AssignmentTarget;
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(id) => Ok(Some(
            PreparedAssignmentTarget::Identifier(id.name.as_str().to_string()),
        )),
        AssignmentTarget::StaticMemberExpression(member) => {
            if matches!(member.object, Expression::Super(_)) {
                let home_reg = load_synthetic_capture(cx, super_home_binding_name(cx), span)?;
                let this_guard = cx.alloc_scratch();
                cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
                let name_idx = cx.intern_string_constant(member.property.name.as_str());
                return Ok(Some(PreparedAssignmentTarget::SuperProperty {
                    home_reg,
                    name_idx,
                }));
            }
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            Ok(Some(PreparedAssignmentTarget::StaticMember {
                obj_reg,
                name_idx,
            }))
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            if matches!(member.object, Expression::Super(_)) {
                let home_reg = load_synthetic_capture(cx, super_home_binding_name(cx), span)?;
                let this_guard = cx.alloc_scratch();
                cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
                let idx_reg = compile_expr(cx, &member.expression, span)?;
                return Ok(Some(PreparedAssignmentTarget::SuperElement {
                    home_reg,
                    idx_reg,
                }));
            }
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let key_reg = compile_expr(cx, &member.expression, span)?;
            Ok(Some(PreparedAssignmentTarget::ComputedMember {
                obj_reg,
                key_reg,
            }))
        }
        AssignmentTarget::PrivateFieldExpression(member) => {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            crate::class::emit_private_method_brand_check(
                cx,
                obj_reg,
                member.field.name.as_str(),
                span,
            )?;
            let key_reg = crate::class::load_private_key(cx, member.field.name.as_str(), span)?;
            Ok(Some(PreparedAssignmentTarget::PrivateField {
                obj_reg,
                key_reg,
            }))
        }
        AssignmentTarget::ArrayAssignmentTarget(_)
        | AssignmentTarget::ObjectAssignmentTarget(_) => Ok(None),
        other => Err(CompileError::Unsupported {
            node: format!("AssignmentTarget ({other:?})"),
            span,
        }),
    }
}

fn assign_prepared_target(
    cx: &mut Compiler,
    target: PreparedAssignmentTarget,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    match target {
        PreparedAssignmentTarget::Identifier(name) => store_identifier(cx, &name, value_reg, span),
        PreparedAssignmentTarget::StaticMember { obj_reg, name_idx } => {
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(value_reg),
                    Operand::Register(scratch),
                ],
                span,
            );
            Ok(())
        }
        PreparedAssignmentTarget::PrivateField { obj_reg, key_reg } => {
            cx.emit(
                Op::PrivateSet,
                vec![
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                    Operand::Register(value_reg),
                ],
                span,
            );
            Ok(())
        }
        PreparedAssignmentTarget::ComputedMember { obj_reg, key_reg } => {
            cx.emit_store_element(obj_reg, key_reg, value_reg, span);
            Ok(())
        }
        PreparedAssignmentTarget::SuperProperty { home_reg, name_idx } => {
            cx.emit(
                Op::SetSuperProperty,
                vec![
                    Operand::Register(home_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(value_reg),
                ],
                span,
            );
            Ok(())
        }
        PreparedAssignmentTarget::SuperElement { home_reg, idx_reg } => {
            cx.emit(
                Op::SetSuperElement,
                vec![
                    Operand::Register(home_reg),
                    Operand::Register(idx_reg),
                    Operand::Register(value_reg),
                ],
                span,
            );
            Ok(())
        }
    }
}

fn prepare_maybe_default_target(
    cx: &mut Compiler,
    target: &oxc_ast::ast::AssignmentTargetMaybeDefault<'_>,
    span: (u32, u32),
) -> Result<Option<PreparedAssignmentTarget>, CompileError> {
    use oxc_ast::ast::AssignmentTargetMaybeDefault;
    let inner = match target {
        AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => &d.binding,
        other => other
            .as_assignment_target()
            .ok_or_else(|| CompileError::Unsupported {
                node: format!("AssignmentTargetMaybeDefault ({other:?})"),
                span,
            })?,
    };
    prepare_assignment_target(cx, inner, span)
}

fn assign_maybe_default_with_prepared(
    cx: &mut Compiler,
    target: &oxc_ast::ast::AssignmentTargetMaybeDefault<'_>,
    prepared: Option<PreparedAssignmentTarget>,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    use oxc_ast::ast::AssignmentTargetMaybeDefault;
    match target {
        AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
            let inferred_name = match &d.binding {
                oxc_ast::ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                    Some(id.name.as_str())
                }
                _ => None,
            };
            let resolved = apply_default_with_name(cx, value_reg, &d.init, inferred_name, span)?;
            match prepared {
                Some(target) => assign_prepared_target(cx, target, resolved, span),
                None => assign_to_target(cx, &d.binding, resolved, span),
            }
        }
        other => {
            let inner = other
                .as_assignment_target()
                .ok_or_else(|| CompileError::Unsupported {
                    node: format!("AssignmentTargetMaybeDefault ({other:?})"),
                    span,
                })?;
            match prepared {
                Some(target) => assign_prepared_target(cx, target, value_reg, span),
                None => assign_to_target(cx, inner, value_reg, span),
            }
        }
    }
}

/// Apply `value_reg` to a `ArrayAssignmentTarget`. Walks the
/// iterator, assigns each yielded value, and closes non-exhausted
/// iterators on normal completion. Defaults (`= expr`) substitute
/// when the element is `undefined`. Rest elements (`...rest`) drain
/// the iterator into a fresh array.
pub(crate) fn assign_array_pattern(
    cx: &mut Compiler,
    arr: &oxc_ast::ast::ArrayAssignmentTarget<'_>,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    emit_require_object_coercible(cx, value_reg, span);

    let iter_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetIterator,
        [Operand::Register(iter_reg), Operand::Register(value_reg)],
        span,
    );
    cx.emit(Op::IteratorCloseStart, [Operand::Register(iter_reg)], span);
    let mut last_done_reg = None;
    for element in &arr.elements {
        let prepared = match element {
            Some(element) => prepare_maybe_default_target(cx, element, span)?,
            None => None,
        };
        let val_reg = cx.alloc_scratch();
        let done_reg = cx.alloc_scratch();
        cx.emit(
            Op::IteratorNext,
            [
                Operand::Register(val_reg),
                Operand::Register(done_reg),
                Operand::Register(iter_reg),
            ],
            span,
        );
        last_done_reg = Some(done_reg);
        let Some(element) = element else { continue };
        let elem_span = span;
        assign_maybe_default_with_prepared(cx, element, prepared, val_reg, elem_span)?;
    }
    if let Some(rest) = arr.rest.as_ref() {
        let prepared_rest = prepare_assignment_target(cx, &rest.target, span)?;
        let collected = cx.alloc_scratch();
        cx.emit(
            Op::NewArray,
            [Operand::Register(collected), Operand::ConstIndex(0)],
            span,
        );
        let val_reg = cx.alloc_scratch();
        let done_reg = cx.alloc_scratch();
        let loop_top = cx.next_pc;
        cx.emit(
            Op::IteratorNext,
            [
                Operand::Register(val_reg),
                Operand::Register(done_reg),
                Operand::Register(iter_reg),
            ],
            span,
        );
        let exit = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
        cx.emit(
            Op::ArrayPush,
            [Operand::Register(collected), Operand::Register(val_reg)],
            span,
        );
        let back = cx.emit_branch_placeholder(Op::Jump, None, span);
        cx.patch_branch(back, loop_top);
        cx.patch_branch_to_here(exit);
        match prepared_rest {
            Some(target) => assign_prepared_target(cx, target, collected, span)?,
            None => assign_to_target(cx, &rest.target, collected, span)?,
        }
        cx.emit(Op::IteratorCloseEnd, [Operand::Register(iter_reg)], span);
    } else if let Some(done_reg) = last_done_reg {
        let skip_close = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
        cx.emit(Op::IteratorClose, [Operand::Register(iter_reg)], span);
        cx.patch_branch_to_here(skip_close);
        cx.emit(Op::IteratorCloseEnd, [Operand::Register(iter_reg)], span);
    } else {
        cx.emit(Op::IteratorClose, [Operand::Register(iter_reg)], span);
        cx.emit(Op::IteratorCloseEnd, [Operand::Register(iter_reg)], span);
    }
    Ok(())
}

/// Apply `value_reg` to an `ObjectAssignmentTarget`.
pub(crate) fn assign_object_pattern(
    cx: &mut Compiler,
    obj: &oxc_ast::ast::ObjectAssignmentTarget<'_>,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    use oxc_ast::ast::{AssignmentTargetProperty, PropertyKey};
    emit_require_object_coercible(cx, value_reg, span);

    enum ExtractedKey {
        Static(String),
        Runtime(u16),
    }
    let mut extracted_keys: Vec<ExtractedKey> = Vec::new();
    for prop in &obj.properties {
        match prop {
            AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                let pspan = span;
                let name = p.binding.name.as_str().to_string();
                let val = cx.alloc_scratch();
                cx.emit_load_property(val, value_reg, &name, pspan);
                // §13.15.5.5 — `{ x = anonymousFn }` applies
                // NamedEvaluation so the default function/class is named
                // after the bound identifier.
                let final_val = match p.init.as_ref() {
                    Some(default) => apply_default_with_name(cx, val, default, Some(&name), pspan)?,
                    None => val,
                };
                store_identifier(cx, &name, final_val, pspan)?;
                extracted_keys.push(ExtractedKey::Static(name));
            }
            AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                let pspan = span;
                // §13.15.5.6 KeyedDestructuringAssignmentEvaluation
                // step 1 — the target Reference (member bases, a
                // derived-constructor `this`, computed keys) evaluates
                // BEFORE GetV reads the source property.
                let prepared = prepare_maybe_default_target(cx, &p.binding, pspan)?;
                let val = cx.alloc_scratch();
                if p.computed {
                    let key_reg = match &p.name {
                        PropertyKey::StaticIdentifier(id) => {
                            let r = cx.alloc_scratch();
                            let s = cx.intern_string_constant(id.name.as_str());
                            cx.emit(
                                Op::LoadString,
                                [Operand::Register(r), Operand::ConstIndex(s)],
                                pspan,
                            );
                            r
                        }
                        _ => compile_expr_as_property_key(cx, &p.name, pspan)?,
                    };
                    cx.emit(
                        Op::LoadElement,
                        vec![
                            Operand::Register(val),
                            Operand::Register(value_reg),
                            Operand::Register(key_reg),
                        ],
                        pspan,
                    );
                    extracted_keys.push(ExtractedKey::Runtime(key_reg));
                } else {
                    let key_str = match &p.name {
                        PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                        PropertyKey::StringLiteral(lit) => lit.value.to_string(),
                        PropertyKey::NumericLiteral(lit) => {
                            numeric_literal_to_property_key(lit.value)
                        }
                        PropertyKey::BigIntLiteral(lit) => lit
                            .raw
                            .as_ref()
                            .map(|s| s.as_str())
                            .unwrap_or("")
                            .trim_end_matches('n')
                            .to_string(),
                        other => {
                            return Err(CompileError::Unsupported {
                                node: format!("ObjectAssignmentTarget: non-string key ({other:?})"),
                                span: pspan,
                            });
                        }
                    };
                    cx.emit_load_property(val, value_reg, &key_str, pspan);
                    extracted_keys.push(ExtractedKey::Static(key_str));
                }
                assign_maybe_default_with_prepared(cx, &p.binding, prepared, val, pspan)?;
            }
        }
    }
    if let Some(rest) = obj.rest.as_ref() {
        // §13.15.5 RestObjectAssignment — same shape as the
        // BindingPattern variant.
        let rest_obj = cx.alloc_scratch();
        cx.emit(Op::NewObject, [Operand::Register(rest_obj)], span);
        cx.emit(
            Op::CopyDataProperties,
            [Operand::Register(rest_obj), Operand::Register(value_reg)],
            span,
        );
        for key in &extracted_keys {
            match key {
                ExtractedKey::Static(s) => {
                    let key_const = cx.intern_string_constant(s);
                    let del_dst = cx.alloc_scratch();
                    cx.emit(
                        Op::DeleteProperty,
                        vec![
                            Operand::Register(del_dst),
                            Operand::Register(rest_obj),
                            Operand::ConstIndex(key_const),
                        ],
                        span,
                    );
                }
                ExtractedKey::Runtime(key_reg) => {
                    let del_dst = cx.alloc_scratch();
                    cx.emit(
                        Op::DeleteElement,
                        vec![
                            Operand::Register(del_dst),
                            Operand::Register(rest_obj),
                            Operand::Register(*key_reg),
                        ],
                        span,
                    );
                }
            }
        }
        assign_to_target(cx, &rest.target, rest_obj, span)?;
    }
    Ok(())
}

pub(crate) fn apply_default_with_name(
    cx: &mut Compiler,
    value_reg: u16,
    init: &oxc_ast::ast::Expression<'_>,
    inferred_name: Option<&str>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    // Test `value_reg !== undefined` and pick. Strict identity, not
    // `typeof` — the Annex B IsHTMLDDA host object reports typeof
    // "undefined" yet must NOT trigger destructuring defaults.
    let undef_reg = cx.alloc_scratch();
    cx.emit(Op::LoadUndefined, [Operand::Register(undef_reg)], span);
    let is_undef = cx.alloc_scratch();
    cx.emit(
        Op::Equal,
        vec![
            Operand::Register(is_undef),
            Operand::Register(value_reg),
            Operand::Register(undef_reg),
        ],
        span,
    );
    let result = cx.alloc_scratch();
    // Default branch: if !is_undef (i.e. value defined) jump to
    // the "use value" arm; fall through into "use init".
    let jump_to_use_value = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(is_undef), span);
    let init_val = match inferred_name {
        Some(name) => compile_expr_with_inferred_name(cx, init, name, span)?,
        None => compile_expr(cx, init, span)?,
    };
    cx.emit(
        Op::StoreLocal,
        [Operand::Register(init_val), Operand::Imm32(result as i32)],
        span,
    );
    let jump_to_end = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch_to_here(jump_to_use_value);
    cx.emit(
        Op::StoreLocal,
        [Operand::Register(value_reg), Operand::Imm32(result as i32)],
        span,
    );
    cx.patch_branch_to_here(jump_to_end);
    Ok(result)
}

/// Store `value_reg` into the binding (or globalThis) for `name`.
/// Mirrors the identifier-store branch of `compile_assignment` but
/// without the compound-op handling.
pub(crate) fn store_identifier(
    cx: &mut Compiler,
    name: &str,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    // `store_identifier` is shared between binding *initialization*
    // (rest parameters, `var` / `for` destructuring heads — where the
    // target is legitimately uninitialized at the store) and genuine
    // assignment (destructuring-assignment leaves). Only a cross-function
    // capture needs the runtime TDZ store check; the static TDZ for a
    // same-function `let` is enforced at its reference site.
    let mut capture_store = false;
    let storage = match cx.lookup_binding(name) {
        Some(info) if info.is_const => {
            emit_assignment_type_error(
                cx,
                &format!("Assignment to constant variable `{name}`"),
                span,
            );
            return Ok(());
        }
        Some(info) => Some(info.storage),
        None => {
            let captured_const = cx.stack.iter().rev().skip(1).any(|frame| {
                frame
                    .scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.bindings.get(name))
                    .is_some_and(|info| info.is_const)
            });
            if captured_const {
                emit_assignment_type_error(
                    cx,
                    &format!("Assignment to constant variable `{name}`"),
                    span,
                );
                return Ok(());
            }
            let captured = cx
                .resolve_capture(name)
                .map(|idx| BindingStorage::Upvalue { idx });
            capture_store = captured.is_some();
            captured
        }
    };
    match storage {
        Some(s) => {
            if capture_store {
                cx.emit_assign_storage(value_reg, s, span);
            } else {
                cx.emit_store_storage(value_reg, s, span);
            }
            cx.mark_initialized(name);
            cx.emit_module_export_mirror(name, value_reg, span);
        }
        None => {
            // §10.2.4.2 PutValue — inside a sloppy function with a
            // direct eval the name may resolve to an eval-introduced
            // frame binding before the global environment.
            if cx.any_enclosing_direct_eval() && !cx.is_strict {
                let name_idx = cx.intern_string_constant(name);
                cx.emit(
                    Op::StoreDynamic,
                    [Operand::Register(value_reg), Operand::ConstIndex(name_idx)],
                    span,
                );
                return Ok(());
            }
            // §9.1.1.4 global SetMutableBinding — the declarative
            // record (script lexicals, `const` → TypeError, TDZ →
            // ReferenceError) shadows the object record; strict
            // writes to a missing binding throw ReferenceError at
            // runtime.
            let name_idx = cx.intern_string_constant(name);
            let strict = i32::from(cx.is_strict);
            cx.emit(
                Op::StoreGlobalBinding,
                [
                    Operand::Register(value_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Imm32(strict),
                ],
                span,
            );
        }
    }
    Ok(())
}

pub(crate) fn emit_assignment_type_error(
    cx: &mut Compiler,
    message: &str,
    span: (u32, u32),
) -> u16 {
    let message_reg = cx.alloc_scratch();
    let message = cx.intern_string_constant(message);
    cx.emit(
        Op::LoadString,
        [Operand::Register(message_reg), Operand::ConstIndex(message)],
        span,
    );
    let error_reg = cx.alloc_scratch();
    let kind = cx.intern_string_constant("TypeError");
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

pub(crate) fn emit_reference_error(cx: &mut Compiler, name: &str, span: (u32, u32)) -> u16 {
    let message_reg = cx.alloc_scratch();
    let message = cx.intern_string_constant(&format!("{name} is not defined"));
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

/// Map a compound `AssignmentOperator` to the bytecode binop used
/// by `compile_assignment`. Returns `None` for plain `=` (which
/// skips the load-modify-store sequence) and for logical assigns
/// which the foundation slice doesn't lower yet.
pub(crate) fn compound_assign_op(op: AssignmentOperator) -> Option<Op> {
    Some(match op {
        AssignmentOperator::Assign => return None,
        AssignmentOperator::Addition => Op::Add,
        AssignmentOperator::Subtraction => Op::Sub,
        AssignmentOperator::Multiplication => Op::Mul,
        AssignmentOperator::Division => Op::Div,
        AssignmentOperator::Remainder => Op::Rem,
        AssignmentOperator::Exponential => Op::Pow,
        AssignmentOperator::ShiftLeft => Op::Shl,
        AssignmentOperator::ShiftRight => Op::Shr,
        AssignmentOperator::ShiftRightZeroFill => Op::Ushr,
        AssignmentOperator::BitwiseOR => Op::BitwiseOr,
        AssignmentOperator::BitwiseXOR => Op::BitwiseXor,
        AssignmentOperator::BitwiseAnd => Op::BitwiseAnd,
        AssignmentOperator::LogicalOr
        | AssignmentOperator::LogicalAnd
        | AssignmentOperator::LogicalNullish => return None,
    })
}

/// Walk an `AssignmentTarget` AST (destructuring patterns recurse
/// through nested array / object targets) and raise
/// `CompileError::Syntax` on any strict-reserved identifier used as
/// an `AssignmentTargetIdentifier`. Caller decides when strict-mode
/// applies.
pub(crate) fn validate_strict_assignment_target(
    target: &oxc_ast::ast::AssignmentTarget<'_>,
) -> Result<(), CompileError> {
    use oxc_ast::ast::{AssignmentTarget, AssignmentTargetProperty, AssignmentTargetRest};
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(id) => {
            let name = id.name.as_str();
            if is_strict_reserved_binding_name(name) {
                return Err(CompileError::Syntax {
                    messages: vec![format!(
                        "SyntaxError: '{name}' is not a valid assignment target in strict mode"
                    )],
                    diagnostics: Vec::new(),
                });
            }
        }
        AssignmentTarget::ArrayAssignmentTarget(arr) => {
            for t in arr.elements.iter().flatten() {
                validate_strict_assignment_target_maybe_default(t)?;
            }
            if let Some(rest) = &arr.rest {
                let AssignmentTargetRest { target, .. } = rest.as_ref();
                validate_strict_assignment_target(target)?;
            }
        }
        AssignmentTarget::ObjectAssignmentTarget(obj) => {
            for prop in &obj.properties {
                match prop {
                    AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                        let name = p.binding.name.as_str();
                        if is_strict_reserved_binding_name(name) {
                            return Err(CompileError::Syntax {
                                messages: vec![format!(
                                    "SyntaxError: '{name}' is not a valid assignment target in strict mode"
                                )],
                                diagnostics: Vec::new(),
                            });
                        }
                    }
                    AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                        validate_strict_assignment_target_maybe_default(&p.binding)?;
                    }
                }
            }
            if let Some(rest) = &obj.rest {
                let AssignmentTargetRest { target, .. } = rest.as_ref();
                validate_strict_assignment_target(target)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn validate_strict_assignment_target_maybe_default(
    target: &oxc_ast::ast::AssignmentTargetMaybeDefault<'_>,
) -> Result<(), CompileError> {
    use oxc_ast::ast::AssignmentTargetMaybeDefault;
    match target {
        AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
            validate_strict_assignment_target(&d.binding)
        }
        other => {
            if let Some(t) = other.as_assignment_target() {
                validate_strict_assignment_target(t)
            } else {
                Ok(())
            }
        }
    }
}
