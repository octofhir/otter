//! Try, catch, and finally statement lowering.
//!
//! # Contents
//! - try-region emission
//! - catch binding setup
//! - finally finalization
//!
//! # Invariants
//! - Every entered try region is paired with explicit leave or finalizer handling.
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

        cx.enter_scope();
        for inner_stmt in &s.block.body {
            compile_statement(cx, inner_stmt)?;
        }
        cx.exit_scope();
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
        cx.enter_scope();
        for inner_stmt in &s.block.body {
            compile_statement(cx, inner_stmt)?;
        }
        cx.exit_scope();
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
    cx.enter_scope();
    for inner_stmt in &s.block.body {
        compile_statement(cx, inner_stmt)?;
    }
    cx.exit_scope();
    cx.emit(Op::LeaveTry, vec![], span);
    cx.active_handlers -= 1;
    cx.active_finally -= 1;
    cx.patch_enter_try_offset(handler_pc, false);
    compile_finalizer(cx, s.finalizer.as_ref().unwrap())?;
    cx.emit(Op::EndFinally, vec![], span);
    Ok(None)
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
    cx.enter_scope();
    if let Some(param) = &handler.param {
        match &param.pattern {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) => {
                let pname = id.name.as_str().to_string();
                let storage = cx.declare_binding(&pname, false, span)?;
                cx.emit_store_storage(exc_reg, storage, span);
                cx.mark_initialized(&pname);
            }
            // §14.15 Catch — `catch (pattern) { … }` accepts a
            // BindingPattern. Destructure the exception value into
            // freshly-declared lexical bindings.
            // <https://tc39.es/ecma262/#sec-runtime-semantics-catchclauseevaluation>
            _ => destructure_into(cx, exc_reg, &param.pattern, span)?,
        }
    }
    for inner in &handler.body.body {
        compile_statement(cx, inner)?;
    }
    cx.exit_scope();
    Ok(())
}

pub(crate) fn compile_finalizer(
    cx: &mut Compiler,
    finalizer: &oxc_ast::ast::BlockStatement<'_>,
) -> Result<(), CompileError> {
    // §14.15.3 step 4 — a normal finalizer completion value is
    // discarded; its statements must not touch the program
    // completion register.
    let saved = cx.completion_suppressed;
    cx.top_mut().completion_suppressed = true;
    cx.enter_scope();
    let result: Result<(), CompileError> = (|| {
        for inner in &finalizer.body {
            compile_statement(cx, inner)?;
        }
        Ok(())
    })();
    cx.exit_scope();
    cx.top_mut().completion_suppressed = saved;
    result
}
