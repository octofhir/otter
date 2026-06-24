//! `for-in` and `for-of` loop lowering helpers.
//!
//! # Contents
//! - for-of iterator lowering
//! - for-in property iteration lowering
//! - loop-head binding setup
//!
//! # Invariants
//! - Loop frames own their break and continue patch sites.
//!
//! # See also
//! - `statements` for general statement dispatch

use crate::*;

/// Lower `for (let x of expr) { body }` to the foundation
/// iterator-protocol shape:
///
/// ```text
///   tmp_iter = GetIterator(expr)
///   loop_top:
///   IteratorNext value, done, tmp_iter
///   JumpIfTrue done -> loop_exit
///   <bind value into the loop variable>
///   <body>
///   Jump -> loop_top
///   loop_exit:
/// ```
///
/// The loop variable lives in a fresh scope per iteration so a
/// `let`-declared binding does not leak between iterations or to
/// the outside. `break` lands at `loop_exit`; `continue` jumps to
/// the top so a fresh value is fetched. Real iterator-close
/// semantics (running an `[@@return]` hook on early termination)
/// land alongside generators in a later slice.
pub(crate) fn compile_for_of_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::ForOfStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    cx.emit_completion_reset(span);
    let is_for_await = s.r#await;

    // §14.7.5 — a `for (let … of RHS)` / `for (const … of RHS)` head
    // binds each name (single identifier or destructuring leaf) in an
    // own-upvalue cell that `Op::FreshUpvalue` re-installs as a hole.
    // This gives both the head Temporal Dead Zone during RHS
    // evaluation (§14.7.5.12) and a fresh binding per iteration for
    // any capturing closure (§14.7.5.6 CreatePerIterationEnvironment).
    // `var` and assignment-target heads keep the register path in
    // `bind_for_in_of_head`.
    let per_iter_upvalues = declare_per_iteration_head(cx, &s.left, span)?;

    let iterable_reg = compile_expr(cx, &s.right, span)?;
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

    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();

    // §14.7.5.6 ForIn/OfBodyEvaluation maintains a running completion
    // value `V`, updated to each non-empty body completion and
    // returned as the statement's value. Initialise to `undefined`
    // (the result when the body runs zero times or produces no value).
    let completion_reg = cx.alloc_scratch();
    cx.emit(Op::LoadUndefined, [Operand::Register(completion_reg)], span);

    cx.push_loop_frame(LoopFrame::iteration());
    // §7.4.9 — register this iterator so abrupt completions (`break`,
    // labelled `continue`, `return`) that exit the loop emit
    // IteratorClose at the jump site. Async iterators need
    // AsyncIteratorClose (await on the result) which the synchronous
    // close path cannot perform, so leave `for await` to the existing
    // break-only handling below.
    if !is_for_await && let Some(frame) = cx.loops.last_mut() {
        frame.iterator_close_reg = Some(iter_reg);
    }
    // §7.4.9 — open the iterator's close region so a throw inside the
    // body runs its `[[return]]` during unwind (`IteratorCloseEnd` at
    // the loop exit closes the region on normal / `break` completion;
    // an exhausted iterator is already done and must not be re-closed).
    // `break` / `continue` / `return` close inline at the jump site, so
    // this region only covers the dynamic throw-unwind path.
    if !is_for_await {
        cx.emit(Op::IteratorCloseStart, [Operand::Register(iter_reg)], span);
    }
    let loop_top = cx.next_pc;
    // §14.7.5.6 — materialise fresh per-iteration cells for a captured
    // `let`/`const` head before the next value binds, so each
    // iteration's closures capture distinct bindings.
    for &idx in &per_iter_upvalues {
        cx.emit(Op::FreshUpvalue, [Operand::Imm32(idx as i32)], span);
    }
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

    // §14.7.5.6 step `for await … of` — async iterators already
    // produced an awaited result record above; ordinary `for-of`
    // uses the synchronous `IteratorNext` value directly.
    // <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements>
    let bind_source = value_reg;

    // §14.7.5.6 ForIn/OfBodyEvaluation: `let`/`const` re-bind per
    // iteration in a fresh lexical scope; `var` writes back into
    // the function-scope binding pre-hoisted at function entry.
    // AssignmentTarget heads reassign without a fresh scope per
    // step (no per-iteration binding to materialize).
    cx.enter_scope();
    bind_for_in_of_head(cx, &s.left, bind_source, span)?;
    if let Some(body_reg) = compile_statement(cx, &s.body)? {
        // Record this iteration's non-empty completion as `V`. A
        // `break` / `continue` jumps out of the body before reaching
        // here, so `V` keeps the prior iteration's value per spec.
        cx.emit(
            Op::StoreLocal,
            [
                Operand::Register(body_reg),
                Operand::Imm32(completion_reg as i32),
            ],
            span,
        );
    }
    cx.exit_scope();

    let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch(back_jmp, loop_top);

    let frame = cx.loops.pop().expect("for-of loop frame");
    // `continue` re-iterates without closing the iterator (§14.7.5.6 —
    // a continue completion is not abrupt with respect to the loop).
    for pc in frame.continue_patches {
        cx.patch_branch(pc, loop_top);
    }
    // §14.7.5.6 ForIn/OfBodyEvaluation — a `break` is an abrupt
    // completion that must run IteratorClose. For synchronous `for…of`
    // the close is emitted at the `break` / labelled-`continue` /
    // `return` site (see `compile_break_with_iterator_close` and
    // friends), so the break target here just lands at the exit. The
    // exhausted-iterator exit (`done` true) must NOT close. `for await`
    // still closes at the target (its sites are not annotated, pending
    // AsyncIteratorClose support).
    let had_breaks = !frame.break_patches.is_empty();
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    if is_for_await && had_breaks {
        cx.emit(Op::IteratorClose, [Operand::Register(iter_reg)], span);
    }
    cx.patch_branch_to_here(exit_jmp);
    // Close the throw-unwind region: both the exhausted-iterator exit
    // and a `break` reach here. Removing the registration prevents an
    // already-finished (or inline-closed) iterator from being closed a
    // second time by a later throw further up the same frame.
    if !is_for_await {
        cx.emit(Op::IteratorCloseEnd, [Operand::Register(iter_reg)], span);
    }
    // Close the head-binding scope opened for the per-iteration
    // `let`/`const` cell — leaving it pushed would leak the head name
    // into the enclosing scope's redeclaration checks.
    if !per_iter_upvalues.is_empty() {
        cx.exit_scope();
    }
    Ok(Some(completion_reg))
}

/// If `head` is a `for (let x of …)` / `for (const x of …)` single
/// identifier binding, return `(name, is_const)`. Such heads route
/// through a per-iteration upvalue cell (§14.7.5.6) that also provides
/// the head Temporal Dead Zone (§14.7.5.12). `var`, destructuring, and
/// AssignmentTarget heads return `None` and keep the register path.
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
    // §14.7.5 — a single-identifier `let`/`const` head binds in an
    // own-upvalue cell holed during RHS evaluation (head TDZ) and
    // re-installed fresh per iteration, mirroring the for-of path.
    let per_iter_upvalues = declare_per_iteration_head(cx, &s.left, span)?;

    let obj_reg = compile_expr(cx, &s.right, span)?;
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
    let loop_top = cx.next_pc;
    // §14.7.5.6 CreatePerIterationEnvironment — fresh cell before the
    // next key binds so closures capture a distinct binding.
    for &idx in &per_iter_upvalues {
        cx.emit(Op::FreshUpvalue, [Operand::Imm32(idx as i32)], span);
    }
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
    cx.enter_scope();
    bind_for_in_of_head(cx, &s.left, value_reg, span)?;
    compile_statement(cx, &s.body)?;
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
    if !per_iter_upvalues.is_empty() {
        cx.exit_scope();
    }
    Ok(None)
}

/// Open the head-binding scope and declare each `let`/`const` head
/// name (single identifier or destructuring leaf) in a holed
/// own-upvalue cell — §14.7.5.12 head TDZ during RHS evaluation plus
/// per-iteration freshness. Returns the cell indices (empty for `var`
/// / assignment-target heads, which keep the register path).
fn declare_per_iteration_head(
    cx: &mut Compiler,
    head: &oxc_ast::ast::ForStatementLeft<'_>,
    span: (u32, u32),
) -> Result<Vec<u16>, CompileError> {
    let names = per_iteration_head_names(head);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    cx.enter_scope();
    let mut cells = Vec::with_capacity(names.len());
    for (name, is_const) in &names {
        let idx = match cx.declare_captured_binding(name, *is_const, span)? {
            crate::scope::BindingStorage::Upvalue { idx } => idx,
            crate::scope::BindingStorage::Register { .. } => {
                unreachable!("declare_captured_binding always yields an upvalue")
            }
        };
        // Hole the cell so closures inside the RHS observe the TDZ.
        cx.emit(Op::FreshUpvalue, [Operand::Imm32(idx as i32)], span);
        cells.push(idx);
    }
    Ok(cells)
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
                    let storage = if is_var {
                        cx.lookup_binding(&name)
                            .ok_or(CompileError::Unsupported {
                                node: format!("for-of var `{name}` not pre-hoisted"),
                                span,
                            })?
                            .storage
                    } else if let Some(info) = cx.lookup_binding(&name).filter(|info| {
                        !info.initialized
                            && matches!(info.storage, crate::scope::BindingStorage::Upvalue { .. })
                    }) {
                        // §14.7.5.6 — the single-identifier head's
                        // per-iteration cell was pre-declared (holed)
                        // by the loop prologue; bind THROUGH it so
                        // Op::FreshUpvalue re-installs the binding
                        // every iteration and closures capture a
                        // distinct copy.
                        info.storage
                    } else {
                        cx.declare_binding(&name, is_const, span)?
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
            // `for (super.X of ...)` writes through the receiver per
            // §13.3.5.3 + §6.2.5.5 step 6.b, like `super.X = V`.
            if matches!(member.object, oxc_ast::ast::Expression::Super(_)) {
                let home_reg = crate::class::load_synthetic_capture(
                    cx,
                    crate::class::super_home_binding_name(cx),
                    span,
                )?;
                let this_guard = cx.alloc_scratch();
                cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
                let name_idx = cx.intern_string_constant(member.property.name.as_str());
                cx.emit(
                    Op::SetSuperProperty,
                    vec![
                        Operand::Register(home_reg),
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
            cx.emit(
                Op::StoreProperty,
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
                let home_reg = crate::class::load_synthetic_capture(
                    cx,
                    crate::class::super_home_binding_name(cx),
                    span,
                )?;
                let this_guard = cx.alloc_scratch();
                cx.emit(Op::LoadThis, [Operand::Register(this_guard)], span);
                let key_reg = compile_expr(cx, &member.expression, span)?;
                cx.emit(
                    Op::SetSuperElement,
                    vec![
                        Operand::Register(home_reg),
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
