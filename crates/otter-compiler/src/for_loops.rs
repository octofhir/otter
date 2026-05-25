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
    let is_for_await = s.r#await;

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

    cx.push_loop_frame(LoopFrame::iteration());
    let loop_top = cx.next_pc;
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
    compile_statement(cx, &s.body)?;
    cx.exit_scope();

    let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch(back_jmp, loop_top);
    cx.patch_branch_to_here(exit_jmp);

    let frame = cx.loops.pop().expect("for-of loop frame");
    for pc in frame.continue_patches {
        cx.patch_branch(pc, loop_top);
    }
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    Ok(None)
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

    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();

    cx.push_loop_frame(LoopFrame::iteration());
    let loop_top = cx.next_pc;
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
    Ok(None)
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
                    let storage = if is_var {
                        cx.lookup_binding(&name)
                            .ok_or(CompileError::Unsupported {
                                node: format!("for-of var `{name}` not pre-hoisted"),
                                span,
                            })?
                            .storage
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
        ForStatementLeft::PrivateFieldExpression(_) => Err(CompileError::Unsupported {
            node: "ForOfStatement: private-field target head".to_string(),
            span,
        }),
    }
}
