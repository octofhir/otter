//! `for-in` and `for-of` loop lowering helpers.
//!
//! # Contents
//! - for-of iterator lowering
//! - for-in property iteration lowering
//! - loop-head binding setup
//!
//! # Invariants
//! - Loop frames own their break and continue patch sites.
//! - A `let` / `const` head binds in a fresh `ForHead` scope per iteration
//!   (§14.7.5.6): the scope's context, when one is needed, is created after
//!   `IteratorNext` and before the head binds, with the enclosing context as
//!   its parent. The right-hand side runs in a separate TDZ scope holding the
//!   head names (§14.7.5.12); it owns a context only when a closure or eval
//!   in the right-hand side can observe them.
//!
//! # See also
//! - `statements` for general statement dispatch

use crate::*;

/// Lower `for (x of expr) body` (§14.7.5.6 ForIn/OfBodyEvaluation):
///
/// ```text
///   iter = GetIterator(expr)            ; GetAsyncIterator for `for await`
///   top:
///   IteratorNext value, done, iter      ; `next()` + Await for `for await`
///   JumpIfTrue done -> exit
///   <bind value> <body>                 ; protected: lands at `landing`
///   Jump -> top
///   landing: close iter for a throw, rethrow
///   <one exit per break / continue / return leaving the body:
///    close iter, continue the command outside the loop>
///   exit:
/// ```
///
/// The head is outside the protected range: §7.4.8 leaves an iterator whose
/// `next` or result getters threw alone. `continue` to this loop re-iterates
/// without closing.
pub(crate) fn compile_for_of_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::ForOfStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    cx.emit_completion_reset(span);
    let is_for_await = s.r#await;

    // §14.7.5.12 ForIn/OfHeadEvaluation — the right-hand side runs with
    // the head's `let` / `const` names in their TDZ.
    let head_names = per_iteration_head_names(&s.left);
    let iterable_reg = compile_head_rhs(cx, &head_names, &s.right, span)?;
    let iter_reg = cx.alloc_scratch();
    if is_for_await {
        cx.emit(
            Op::GetAsyncIterator,
            [Operand::Register(iter_reg), Operand::Register(iterable_reg)],
            span,
        );
    } else {
        cx.emit(
            Op::GetIterator,
            [Operand::Register(iter_reg), Operand::Register(iterable_reg)],
            span,
        );
    }
    // The unwinder writes a throw out of the body here.
    let exc_reg = cx.alloc_scratch();
    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();

    // §14.7.5.6 ForIn/OfBodyEvaluation maintains a running completion
    // value `V`, updated to each non-empty body completion and
    // returned as the statement's value. Initialise to `undefined`
    // (the result when the body runs zero times or produces no value).
    let completion_reg = cx.alloc_completion_reg(span);

    cx.push_loop_frame(LoopFrame::iteration());
    cx.enter_iterator_scope(iter_reg, is_for_await);
    let depth = cx.control.len();
    if let Some(frame) = cx.loops.last_mut() {
        frame.continue_depth = depth;
    }
    let loop_top = cx.next_pc();
    if is_for_await {
        let result_reg = cx.alloc_scratch();
        let awaited_reg = cx.alloc_scratch();
        let next_name = cx.intern_string_constant("next");
        cx.emit(
            Op::CallMethodValue,
            vec![
                Operand::Register(result_reg),
                Operand::Register(iter_reg),
                Operand::ConstIndex(next_name),
                Operand::ConstIndex(0),
            ],
            span,
        );
        cx.emit(
            Op::Await,
            [
                Operand::Register(awaited_reg),
                Operand::Register(result_reg),
            ],
            span,
        );
        cx.emit_load_property(done_reg, awaited_reg, "done", span);
        cx.emit_load_property(value_reg, awaited_reg, "value", span);
    } else {
        cx.emit(
            Op::IteratorNext,
            vec![
                Operand::Register(value_reg),
                Operand::Register(done_reg),
                Operand::Register(iter_reg),
            ],
            span,
        );
    }
    let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);

    // §14.7.5.6 ForIn/OfBodyEvaluation: `let`/`const` re-bind per
    // iteration in a fresh lexical scope; `var` writes back into
    // the function-scope binding pre-hoisted at function entry.
    // AssignmentTarget heads reassign without a fresh scope per
    // step (no per-iteration binding to materialize).
    let body_start = cx.next_pc();
    enter_iteration_scope(cx, &head_names, span)?;
    bind_for_in_of_head(cx, &s.left, value_reg, span)?;
    if let Some(body_reg) = compile_statement(cx, &s.body)? {
        // Record this iteration's non-empty completion as `V`. A
        // `break` / `continue` jumps out of the body before reaching
        // here, so `V` keeps the prior iteration's value per spec.
        cx.store_completion(completion_reg, body_reg, span);
    }
    cx.exit_scope();
    let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch(back_jmp, loop_top);
    let landing = cx.protect(body_start, exc_reg);

    // §7.4.11 step 6 — a throw out of the body closes the iterator and
    // answers with the original throw before anything the close produced
    // is looked at.
    if let Some(landing) = landing {
        cx.patch_handler_to_here(landing);
        if is_for_await {
            // The close is an `await`, and whatever it raises is dropped.
            let swallow_exc = cx.alloc_scratch();
            let start = cx.next_pc();
            emit_async_iterator_close(cx, iter_reg, false, span);
            let closed = cx.emit_branch_placeholder(Op::Jump, None, span);
            if let Some(swallow) = cx.protect(start, swallow_exc) {
                cx.patch_handler_to_here(swallow);
            }
            cx.patch_branch_to_here(closed);
        } else {
            cx.emit(Op::IteratorCloseThrow, [Operand::Register(iter_reg)], span);
        }
        cx.emit(Op::Throw, [Operand::Register(exc_reg)], span);
    }
    cx.leave_iterator_scope(span);

    let frame = cx.loops.pop().expect("for-of loop frame");
    // `continue` re-iterates without closing the iterator (§14.7.5.6 —
    // a continue completion is not abrupt with respect to the loop).
    for pc in frame.continue_patches {
        cx.patch_branch(pc, loop_top);
    }
    // A break reaches here after its exit closed the iterator; the
    // exhausted-iterator exit must not close it.
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    cx.patch_branch_to_here(exit_jmp);
    Ok(completion_reg)
}

/// §7.4.11 AsyncIteratorClose — call the iterator's `return` and await its
/// result. `check_result` requires that result to be an Object, which the spec
/// asks for only when there is no throw completion already in flight.
pub(crate) fn emit_async_iterator_close(
    cx: &mut FunctionContext,
    iter_reg: u16,
    check_result: bool,
    span: (u32, u32),
) {
    let result_reg = cx.alloc_scratch();
    let called_reg = cx.alloc_scratch();
    let awaited_reg = cx.alloc_scratch();
    cx.emit(
        Op::AsyncIteratorReturn,
        vec![
            Operand::Register(result_reg),
            Operand::Register(called_reg),
            Operand::Register(iter_reg),
        ],
        span,
    );
    let skip = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(called_reg), span);
    cx.emit(
        Op::Await,
        [
            Operand::Register(awaited_reg),
            Operand::Register(result_reg),
        ],
        span,
    );
    if check_result {
        cx.emit(
            Op::CheckIteratorResult,
            [Operand::Register(awaited_reg)],
            span,
        );
    }
    cx.patch_branch_to_here(skip);
}

/// The `(name, is_const)` leaves of a `for (let … of …)` /
/// `for (const … in …)` head. `var` and AssignmentTarget heads bind no
/// per-iteration names and return nothing.
fn per_iteration_head_names(head: &oxc_ast::ast::ForStatementLeft<'_>) -> Vec<(String, bool)> {
    use oxc_ast::ast::{ForStatementLeft, VariableDeclarationKind};
    let ForStatementLeft::VariableDeclaration(decl) = head else {
        return Vec::new();
    };
    let is_const = match decl.kind {
        VariableDeclarationKind::Let => false,
        VariableDeclarationKind::Const => true,
        _ => return Vec::new(),
    };
    if decl.declarations.len() != 1 {
        return Vec::new();
    }
    let mut leaves: Vec<String> = Vec::new();
    crate::hoist::collect_pattern_var_names(&decl.declarations[0].id, &mut leaves);
    leaves.into_iter().map(|n| (n, is_const)).collect()
}

/// Lower `for (k in obj) { … }` per ECMA-262 §14.7.5.6
/// `ForIn/OfHeadEvaluation` + §14.7.5.10 EnumerateObjectProperties.
///
/// # Algorithm
/// 1. Evaluate the right-hand side. If it is `null` / `undefined`
///    the loop is silently skipped (§14.7.5.6 step 7.b).
/// 2. Snapshot the receiver's enumerable own + inherited string
///    keys at loop entry. Foundation keeps the snapshot static —
///    spec §14.7.5.10's "iterate keys created during enumeration"
///    is filed against a follow-up.
/// 3. Walk the snapshot via an integer counter; on each iteration
///    re-bind the loop variable in a fresh per-iteration scope so
///    `let k in o` matches §14.7.5.6 step 7.f.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements>
/// - <https://tc39.es/ecma262/#sec-enumerate-object-properties>
pub(crate) fn compile_for_in_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::ForInStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    cx.emit_completion_reset(span);

    // §B.3.6 — `for (var x = init in expr)`: the legacy initializer
    // evaluates and assigns the var binding BEFORE the enumerated
    // expression (sloppy-only web-compat grammar).
    if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(decl) = &s.left
        && matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var)
        && let Some(declarator) = decl.declarations.first()
        && let Some(init) = &declarator.init
        && let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id
    {
        let name = id.name.as_str().to_string();
        let init_reg = crate::expr::compile_expr_with_inferred_name(cx, init, &name, span)?;
        store_identifier(cx, &name, init_reg, span)?;
    }

    // Lower through the VM's internal for-in enumerable-key snapshot
    // helper. It intentionally does not alias `Object.keys`: `keys`
    // is own-only, while `for-in` walks enumerable string keys across
    // the prototype chain.
    //
    // We emit:
    //   r_obj = <right>;
    //   r_keys = ForInKeys(r_obj);            // spec primitive opcode
    //   r_iter = GetIterator(r_keys);
    //   loop_top:
    //     IteratorNext r_value, r_done, r_iter
    //     JumpIfTrue r_done -> exit
    //     <bind let k = r_value>
    //     <body>
    //     Jump loop_top
    //   exit:
    // §14.7.5.12 — the right-hand side runs with the head names in TDZ.
    let head_names = per_iteration_head_names(&s.left);
    let obj_reg = compile_head_rhs(cx, &head_names, &s.right, span)?;
    let keys_reg = cx.alloc_scratch();
    cx.emit(
        Op::ForInKeys,
        [Operand::Register(keys_reg), Operand::Register(obj_reg)],
        span,
    );

    let iter_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetIterator,
        [Operand::Register(iter_reg), Operand::Register(keys_reg)],
        span,
    );

    // §14.7.5.10 EnumerateObjectProperties — a property deleted
    // before being visited is not visited, so each key from the
    // snapshot re-checks existence against the live object. The
    // check target is ToObject(rhs) (§14.7.5.6 step 6.b); a nullish
    // rhs yields an empty snapshot and never reaches the check, so
    // the coercion is guarded rather than unconditional.
    let check_obj_reg = cx.alloc_scratch();
    cx.emit(
        Op::StoreLocal,
        [
            Operand::Register(obj_reg),
            Operand::Imm32(check_obj_reg as i32),
        ],
        span,
    );
    let skip_to_object = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
    cx.emit(
        Op::ToObject,
        [Operand::Register(check_obj_reg), Operand::Register(obj_reg)],
        span,
    );
    cx.patch_branch_to_here(skip_to_object);

    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();

    cx.push_loop_frame(LoopFrame::iteration());
    let loop_top = cx.next_pc();
    cx.emit(
        Op::IteratorNext,
        vec![
            Operand::Register(value_reg),
            Operand::Register(done_reg),
            Operand::Register(iter_reg),
        ],
        span,
    );
    let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);

    // Deleted-during-enumeration skip (§14.7.5.10): absent keys loop
    // straight back to the next snapshot entry.
    let present_reg = cx.alloc_scratch();
    cx.emit(
        Op::HasProperty,
        vec![
            Operand::Register(present_reg),
            Operand::Register(value_reg),
            Operand::Register(check_obj_reg),
        ],
        span,
    );
    let skip_jmp = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(present_reg), span);
    cx.patch_branch(skip_jmp, loop_top);

    // §14.7.5.6 — `let`/`const` rebinds per iteration; `var`
    // re-uses the function-scope binding. Assignment-target heads
    // reassign in place.
    enter_iteration_scope(cx, &head_names, span)?;
    bind_for_in_of_head(cx, &s.left, value_reg, span)?;
    compile_discarded_statement(cx, &s.body)?;
    cx.exit_scope();

    let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch(back_jmp, loop_top);
    cx.patch_branch_to_here(exit_jmp);

    let frame = cx.loops.pop().expect("for-in loop frame");
    for pc in frame.continue_patches {
        cx.patch_branch(pc, loop_top);
    }
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    Ok(None)
}

/// Evaluate a `for-in` / `for-of` right-hand side. With `let` / `const`
/// head names, it runs inside a TDZ scope declaring them uninitialized
/// (§14.7.5.12); a name a closure or direct eval in the right-hand side can
/// observe takes a slot of that scope's hole-initialized context.
fn compile_head_rhs(
    cx: &mut Compiler,
    names: &[(String, bool)],
    rhs: &Expression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if names.is_empty() {
        return compile_expr(cx, rhs, span);
    }
    let (rhs_refs, rhs_eval) = cx.capture.nested_refs_in_expression(rhs);
    cx.enter_scope(otter_bytecode::ScopeKind::ForHead);
    let result = (|| {
        for (name, is_const) in names {
            if cx.lookup_in_current_scope(name).is_some() {
                continue;
            }
            let observed = rhs_eval || rhs_refs.contains(name);
            cx.declare_binding_with_capture(name, lexical_kind(*is_const), span, observed)?;
        }
        compile_expr(cx, rhs, span)
    })();
    cx.exit_scope();
    result
}

/// Enter one iteration's head scope and declare its `let` / `const`
/// names uninitialized, so the head binding and closures in destructuring
/// defaults resolve them (§14.7.5.6 steps h–j).
fn enter_iteration_scope(
    cx: &mut Compiler,
    names: &[(String, bool)],
    span: (u32, u32),
) -> Result<(), CompileError> {
    cx.enter_scope(otter_bytecode::ScopeKind::ForHead);
    for (name, is_const) in names {
        if cx.lookup_in_current_scope(name).is_none() {
            cx.declare_binding(name, lexical_kind(*is_const), span)?;
        }
    }
    Ok(())
}

/// Bind the per-iteration value of a `for-in` / `for-of` head to the
/// declared / pre-existing target. Handles the four head shapes oxc
/// produces:
///
/// 1. `for (let x of …)` / `const` / `var` with a plain identifier,
/// 2. `for (let [a, b] of …)` etc. with a destructuring pattern,
/// 3. `for (x of …)` — assignment to an existing identifier,
/// 4. `for (obj.prop of …)` / `for ([a, b] of …)` etc. — assignment
///    to a member expression or destructuring assignment target.
///
/// Spec: <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements>
/// (ForIn/OfBodyEvaluation).
pub(crate) fn bind_for_in_of_head(
    cx: &mut Compiler,
    head: &oxc_ast::ast::ForStatementLeft<'_>,
    src_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    use oxc_ast::ast::{BindingPattern, ForStatementLeft, VariableDeclarationKind};
    match head {
        ForStatementLeft::VariableDeclaration(decl) => {
            if decl.declarations.len() != 1 {
                return Err(CompileError::Unsupported {
                    node: "ForOfStatement: multi-declarator head".to_string(),
                    span,
                });
            }
            let declarator = &decl.declarations[0];
            let is_const = matches!(decl.kind, VariableDeclarationKind::Const);
            let is_var = matches!(decl.kind, VariableDeclarationKind::Var);
            match &declarator.id {
                BindingPattern::BindingIdentifier(id) => {
                    let name = id.name.as_str().to_string();
                    // §16.1.7 — script global vars are global-object
                    // properties; the head assignment writes through
                    // the property, not a local slot.
                    if is_var
                        && cx.lookup_binding(&name).is_none()
                        && cx.script_global_vars.contains(&name)
                    {
                        let name_idx = cx.intern_string_constant(&name);
                        cx.emit(
                            Op::DefineGlobalVar,
                            [Operand::ConstIndex(name_idx), Operand::Register(src_reg)],
                            span,
                        );
                        return Ok(());
                    }
                    if is_var {
                        // `var` heads assign the variable-scope binding.
                        return store_identifier(cx, &name, src_reg, span);
                    }
                    // The iteration scope pre-declared the head binding.
                    let storage = match cx.lookup_in_current_scope(&name) {
                        Some(info) => info.storage,
                        None => cx.declare_binding(&name, lexical_kind(is_const), span)?,
                    };
                    cx.emit_store_storage(src_reg, storage, span);
                    cx.mark_initialized(&name);
                    Ok(())
                }
                _ => {
                    if is_var {
                        // §14.7.5.6 step 6.b — for `var` heads, the
                        // pattern leaves were already var-hoisted
                        // at function entry; per iteration we just
                        // assign into those existing bindings.
                        destructure_assign(cx, src_reg, &declarator.id, span)
                    } else {
                        // For let/const heads, declare each leaf
                        // per iteration in the fresh scope.
                        destructure_into(cx, src_reg, &declarator.id, span)
                    }
                }
            }
        }
        // `for (target of …)` — AssignmentTarget. Reuse the
        // shared `assign_to_target` helper which handles
        // identifier / member / array-pattern / object-pattern.
        // We pattern-match each variant explicitly to translate
        // ForStatementLeft → AssignmentTarget without unsafe.
        ForStatementLeft::AssignmentTargetIdentifier(id) => {
            store_identifier(cx, id.name.as_str(), src_reg, span)
        }
        ForStatementLeft::ArrayAssignmentTarget(arr) => {
            assign_array_pattern(cx, arr, src_reg, span)
        }
        ForStatementLeft::ObjectAssignmentTarget(obj) => {
            assign_object_pattern(cx, obj, src_reg, span)
        }
        ForStatementLeft::StaticMemberExpression(member) => {
            if crate::assignment::is_invalid_assignment_target_member(member) {
                let _ =
                    crate::assignment::compile_invalid_assignment_target(cx, &member.object, span)?;
                return Ok(());
            }
            // `for (super.X of ...)` writes through the receiver per
            // §13.3.5.3 + §6.2.5.5 step 6.b, like `super.X = V`.
            if matches!(member.object, oxc_ast::ast::Expression::Super(_)) {
                let this_guard = cx.alloc_scratch();
                cx.emit_load_this(this_guard, span);
                let base_reg = crate::class::emit_super_base(cx, span)?;
                let name_idx = cx.intern_string_constant(member.property.name.as_str());
                cx.emit(
                    Op::SetSuperProperty,
                    vec![
                        Operand::Register(base_reg),
                        Operand::ConstIndex(name_idx),
                        Operand::Register(src_reg),
                    ],
                    span,
                );
                return Ok(());
            }
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let scratch = cx.alloc_scratch();
            let store_op = cx.store_property_op();
            cx.emit(
                store_op,
                vec![
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(src_reg),
                    Operand::Register(scratch),
                ],
                span,
            );
            Ok(())
        }
        ForStatementLeft::ComputedMemberExpression(member) => {
            if matches!(member.object, oxc_ast::ast::Expression::Super(_)) {
                let this_guard = cx.alloc_scratch();
                cx.emit_load_this(this_guard, span);
                let key_reg = compile_expr(cx, &member.expression, span)?;
                let base_reg = crate::class::emit_super_base(cx, span)?;
                cx.emit(
                    Op::SetSuperElement,
                    vec![
                        Operand::Register(base_reg),
                        Operand::Register(key_reg),
                        Operand::Register(src_reg),
                    ],
                    span,
                );
                return Ok(());
            }
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let key_reg = compile_expr(cx, &member.expression, span)?;
            cx.emit_store_element(obj_reg, key_reg, src_reg, span);
            Ok(())
        }
        // TS-only wrapper variants — unwrap the inner target.
        ForStatementLeft::TSAsExpression(_)
        | ForStatementLeft::TSSatisfiesExpression(_)
        | ForStatementLeft::TSNonNullExpression(_)
        | ForStatementLeft::TSTypeAssertion(_) => Err(CompileError::Unsupported {
            node: "ForOfStatement: TS-wrapped target head".to_string(),
            span,
        }),
        ForStatementLeft::PrivateFieldExpression(member) => {
            // §13.15 PutValue on a private reference — brand check,
            // then §7.3.32 PrivateSet (TypeError when the receiver's
            // class did not declare the name).
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
                    Operand::Register(src_reg),
                ],
                span,
            );
            Ok(())
        }
    }
}
