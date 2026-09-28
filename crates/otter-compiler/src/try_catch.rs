//! Try, catch, and finally statement lowering.
//!
//! # Contents
//! - try-region emission
//! - catch binding setup
//! - finally finalization
//!
//! # Invariants
//! - Every entered try region is paired with explicit leave or finalizer handling.
//! - The try, catch, and finally blocks each run BlockDeclarationInstantiation
//!   (§14.2.3): lexical declarations and hoisted functions of the block bind
//!   in its own scope, whose context each entry creates afresh. The catch
//!   parameter shares the catch block's scope (a clash is an early error).
//!
//! # See also
//! - `statements` for dispatch

use crate::*;

/// Lower `try { … } catch (e) { … } finally { … }` per ES spec
/// completion-record semantics (the foundation slice approximates
/// it with a `pending_throw` slot on the frame; see
/// [`Frame::pending_throw`](otter_vm::Frame)). The lowering picks
/// one of three shapes:
///
/// - `try { A } catch (e) { B }` (no finally): one [`Op::EnterTry`]
///   with `catch_pc = C` and `finally_pc = NO_HANDLER_OFFSET`. The
///   try body is followed by [`Op::LeaveTry`] and a forward jump
///   past the catch landing.
/// - `try { A } finally { C }` (no catch): one `EnterTry` with
///   `catch_pc = NO_HANDLER_OFFSET` and `finally_pc = F`. The try
///   body is followed by `LeaveTry` and falls through into `C`,
///   which terminates with [`Op::EndFinally`].
/// - `try { A } catch (e) { B } finally { C }`: two nested
///   `EnterTry`s — the outer one routes any throw inside `A` or
///   `B` through `C`, the inner one routes throws inside `A` to
///   the catch landing. After `B` runs, control falls through into
///   `C`; `EndFinally` re-throws any exception parked on the frame.
///
/// `finally`-rethrow rule (per the task spec): if `finally` itself
/// throws, the new exception replaces the in-flight one. The
/// runtime implements this by overwriting `pending_throw` whenever
/// a fresh `Throw` walks into a finally handler.
pub(crate) fn compile_try_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::TryStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    use otter_bytecode::NO_HANDLER_OFFSET;

    let span = (s.span.start, s.span.end);
    cx.emit_completion_reset(span);
    let has_catch = s.handler.is_some();
    let has_finally = s.finalizer.is_some();
    if !has_catch && !has_finally {
        return Err(CompileError::Unsupported {
            node: "TryStatement without catch or finally".to_string(),
            span,
        });
    }

    // Reserve the exception register up front so its index survives
    // every branch — the unwinder writes the thrown value into it
    // before jumping to the catch landing.
    let exc_reg = cx.alloc_scratch();
    let body_span = (s.block.span.start, s.block.span.end);

    if has_catch && has_finally {
        let outer = cx.emit_enter_try(NO_HANDLER_OFFSET, 0, exc_reg, span);
        // The outer handler carries the `finally`; track both depths so
        // `break`/`continue` inside the body or catch can route through
        // it (§14.15.3).
        cx.active_handlers += 1;
        cx.active_finally += 1;
        let inner = cx.emit_enter_try(0, NO_HANDLER_OFFSET, exc_reg, span);
        cx.active_handlers += 1;

        compile_try_block(cx, &s.block)?;
        cx.emit(Op::LeaveTry, vec![], span);
        cx.active_handlers -= 1; // inner catch handler left
        let success_jump = cx.emit_branch_placeholder(Op::Jump, None, span);

        cx.patch_enter_try_offset(inner, /* catch */ true);
        compile_catch_clause(cx, s.handler.as_ref().unwrap(), exc_reg, body_span)?;

        cx.patch_branch_to_here(success_jump);

        cx.emit(Op::LeaveTry, vec![], span);
        cx.active_handlers -= 1; // outer finally handler left
        cx.active_finally -= 1;
        cx.patch_enter_try_offset(outer, /* finally */ false);
        compile_finalizer(cx, s.finalizer.as_ref().unwrap())?;
        cx.emit(Op::EndFinally, vec![], span);
        return Ok(None);
    }

    if has_catch {
        let handler_pc = cx.emit_enter_try(0, NO_HANDLER_OFFSET, exc_reg, span);
        cx.active_handlers += 1;
        compile_try_block(cx, &s.block)?;
        cx.emit(Op::LeaveTry, vec![], span);
        cx.active_handlers -= 1;
        let skip_catch = cx.emit_branch_placeholder(Op::Jump, None, span);

        cx.patch_enter_try_offset(handler_pc, true);
        compile_catch_clause(cx, s.handler.as_ref().unwrap(), exc_reg, body_span)?;

        cx.patch_branch_to_here(skip_catch);
        return Ok(None);
    }

    // try / finally only.
    let handler_pc = cx.emit_enter_try(NO_HANDLER_OFFSET, 0, exc_reg, span);
    cx.active_handlers += 1;
    cx.active_finally += 1;
    compile_try_block(cx, &s.block)?;
    cx.emit(Op::LeaveTry, vec![], span);
    cx.active_handlers -= 1;
    cx.active_finally -= 1;
    cx.patch_enter_try_offset(handler_pc, false);
    compile_finalizer(cx, s.finalizer.as_ref().unwrap())?;
    cx.emit(Op::EndFinally, vec![], span);
    Ok(None)
}

/// Lower the `try` block as a Block (§14.2.3).
fn compile_try_block(
    cx: &mut Compiler,
    block: &oxc_ast::ast::BlockStatement<'_>,
) -> Result<(), CompileError> {
    let span = (block.span.start, block.span.end);
    let mark = cx.scratch;
    compile_block_body(cx, &block.body, otter_bytecode::ScopeKind::Block, span)?;
    let floor = mark.max(cx.context_register_floor());
    if floor < cx.scratch {
        cx.reset_scratch(floor);
    }
    Ok(())
}

pub(crate) fn compile_catch_clause(
    cx: &mut Compiler,
    handler: &oxc_ast::ast::CatchClause<'_>,
    exc_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    // §14.15.3 — a throw discards the try block's completion value;
    // the catch clause threads its own `V` from `undefined`.
    cx.emit_completion_reset(span);
    // §14.15.2 CatchClauseEvaluation — a fresh catch environment per
    // entry: its context (when the parameter or a block binding lives in
    // a slot) is created here, after the unwinder filled `exc_reg`.
    cx.enter_scope(otter_bytecode::ScopeKind::Catch);
    let result = (|| {
        if let Some(param) = &handler.param {
            match &param.pattern {
                oxc_ast::ast::BindingPattern::BindingIdentifier(id) => {
                    let pname = id.name.as_str().to_string();
                    let storage = cx.declare_binding(
                        &pname,
                        otter_bytecode::SlotKind::CatchParam { simple: true },
                        span,
                    )?;
                    cx.emit_store_storage(exc_reg, storage, span);
                    cx.mark_initialized(&pname);
                }
                // §14.15 Catch — `catch (pattern) { … }` destructures the
                // exception value into fresh bindings of the catch scope.
                // <https://tc39.es/ecma262/#sec-runtime-semantics-catchclauseevaluation>
                pattern => {
                    let mut names = Vec::new();
                    collect_pattern_var_names(pattern, &mut names);
                    for name in &names {
                        if cx.lookup_in_current_scope(name).is_none() {
                            cx.declare_binding(
                                name,
                                otter_bytecode::SlotKind::CatchParam { simple: false },
                                span,
                            )?;
                        }
                    }
                    destructure_into(cx, exc_reg, pattern, span)?;
                }
            }
        }
        // §14.2.3 BlockDeclarationInstantiation of the catch Block — its
        // lexical names and functions bind in the same scope as the
        // parameter.
        compile_block_statements(cx, &handler.body.body, span)?;
        Ok(())
    })();
    cx.exit_scope();
    result
}

pub(crate) fn compile_finalizer(
    cx: &mut Compiler,
    finalizer: &oxc_ast::ast::BlockStatement<'_>,
) -> Result<(), CompileError> {
    // §14.15.3 step 4 — a NORMAL finalizer completion value is
    // discarded, but an abrupt one (break/continue leaving the
    // finalizer) carries the finalizer's own statement values. Lower
    // the body with completion tracking live, snapshot the completion
    // register at entry, and restore it only on the normal fallthrough
    // — abrupt exits skip the restore, keeping the finalizer's value.
    let fin_span = (finalizer.span.start, finalizer.span.end);
    let completion_restore = match cx.completion_reg {
        Some(creg) if !cx.completion_suppressed => {
            let snapshot = cx.top_mut().alloc_scratch();
            cx.emit(
                Op::StoreLocal,
                [Operand::Register(creg), Operand::Imm32(snapshot as i32)],
                fin_span,
            );
            // UpdateEmpty(F, undefined) — an abrupt finalizer exit
            // with no own statement value must surface `undefined`,
            // never the try block's value.
            cx.emit(Op::LoadUndefined, [Operand::Register(creg)], fin_span);
            Some((creg, snapshot))
        }
        _ => None,
    };
    let saved = cx.completion_suppressed;
    cx.top_mut().finally_body_depth += 1;
    // §14.2.3 — the finally Block instantiates its declarations like any
    // other block.
    let fspan = (finalizer.span.start, finalizer.span.end);
    cx.enter_scope(otter_bytecode::ScopeKind::Block);
    let result = compile_block_statements(cx, &finalizer.body, fspan).map(drop);
    cx.exit_scope();
    cx.top_mut().finally_body_depth -= 1;
    cx.top_mut().completion_suppressed = saved;
    if let Some((creg, snapshot)) = completion_restore {
        cx.emit(
            Op::StoreLocal,
            [Operand::Register(snapshot), Operand::Imm32(creg as i32)],
            fin_span,
        );
    }
    result
}
