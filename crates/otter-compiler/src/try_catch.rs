//! Try, catch, and finally statement lowering.
//!
//! # Contents
//! - [`compile_try_statement`] — protected ranges, the catch landing, and
//!   the finally block with its exits.
//! - catch binding setup
//! - finally completion-value handling
//!
//! # Invariants
//! - A catch clause lands from the handler table entry covering its try
//!   block; a finally block is entered from its protected range (try block
//!   and catch clause) with a completion token, then resumes that
//!   completion (see `control`).
//! - The try, catch, and finally blocks each run BlockDeclarationInstantiation
//!   (§14.2.3): lexical declarations and hoisted functions of the block bind
//!   in its own scope, whose context each entry creates afresh. The catch
//!   parameter shares the catch block's scope (a clash is an early error).
//!
//! # See also
//! - `statements` for dispatch
//! - `control` for abrupt-completion routing

use crate::control::ControlScope;
use crate::*;

/// Lower `try { A } catch (e) { B } finally { C }` (§14.15.3).
///
/// - The catch clause is a handler table entry over `A` whose landing binds
///   the thrown value and runs `B`.
/// - The finally block is protected-range code: `A` (and `B`) run inside a
///   finally scope, and every way out of that range — falling out, a throw,
///   a `break` / `continue` / `return` — enters `C` with a token that the
///   code after `C` dispatches on.
pub(crate) fn compile_try_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::TryStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    cx.emit_completion_reset(span);
    let body_span = (s.block.span.start, s.block.span.end);
    let finally_start = s.finalizer.is_some().then(|| cx.enter_finally());

    match &s.handler {
        Some(handler) => {
            // The unwinder writes the thrown value here before the landing.
            let exc_reg = cx.alloc_scratch();
            let start = cx.next_pc();
            cx.control.push(ControlScope::Catch);
            let block = compile_try_block(cx, &s.block);
            cx.control.pop();
            block?;
            // A try block that emits nothing cannot throw: its catch clause
            // is still compiled (its early errors stand) but never entered.
            let landing = cx.protect(start, exc_reg);
            let skip_catch = cx.emit_branch_placeholder(Op::Jump, None, span);
            if let Some(landing) = landing {
                cx.patch_handler_to_here(landing);
            }
            compile_catch_clause(cx, handler, exc_reg, body_span)?;
            cx.patch_branch_to_here(skip_catch);
        }
        None => compile_try_block(cx, &s.block)?,
    }

    if let (Some(start), Some(finalizer)) = (finally_start, &s.finalizer) {
        let scope = cx.leave_finally_range(start, span);
        compile_finalizer(cx, finalizer)?;
        cx.emit_finally_exits(scope, span);
    }
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
    // §14.2.3 — the finally Block instantiates its declarations like any
    // other block.
    let fspan = (finalizer.span.start, finalizer.span.end);
    cx.enter_scope(otter_bytecode::ScopeKind::Block);
    let result = compile_block_statements(cx, &finalizer.body, fspan).map(drop);
    cx.exit_scope();
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
