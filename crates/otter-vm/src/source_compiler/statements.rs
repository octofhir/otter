//! Statement lowering for every non-declaration statement shape:
//! `if` / `while` / `do…while` / `for` / `for…of` / `for…in` /
//! `switch` / `throw` / block / labeled, plus the top-level and
//! nested-statement dispatchers.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Public entry points:
//! `lower_top_statement` (called by classes / using_decl),
//! `lower_nested_statement` (switch / using_decl), and
//! `lower_block_statement` (try_finally).

use super::*;

/// Lowers a single statement at function-body top level. Accepts the
/// full M6 statement surface, including `let`/`const` declarations
/// (which are not allowed inside nested blocks — those go through
/// [`lower_nested_statement`] instead).
pub(super) fn lower_top_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    stmt: &'a Statement<'a>,
) -> Result<(), SourceLoweringError> {
    match stmt {
        Statement::VariableDeclaration(decl) => {
            // D2: `let`/`const` bypass `lower_nested_statement` (the
            // central recording point), so record the starting PC
            // here to keep stack traces / debugger lookups aligned.
            ctx.record_source_location(builder.pc(), stmt.span().start);
            lower_let_const_declaration(builder, ctx, decl)
        }
        // `export const X = ...` at the top level — the compiler's
        // top-level classifier pushes the wrapping
        // `ExportNamedDeclaration` into the script body because
        // the inner `VariableDeclaration` can't be borrowed out of
        // the oxc arena separately. Unwrap it here so the `const`
        // initialiser runs and allocates a local; the synth
        // top-level then flushes the local onto the global object
        // before `capture_exports` harvests the namespace.
        Statement::ExportNamedDeclaration(decl) => {
            ctx.record_source_location(builder.pc(), stmt.span().start);
            match &decl.declaration {
                Some(Declaration::VariableDeclaration(inner)) => {
                    lower_let_const_declaration(builder, ctx, inner)
                }
                Some(Declaration::ClassDeclaration(cls)) => {
                    lower_nested_class_declaration(builder, ctx, cls)
                }
                // `export function` at the top level was already
                // recorded as a regular function declaration by
                // `lower_program` — the synth top-level doesn't
                // need to re-execute it here. Silent no-op.
                Some(Declaration::FunctionDeclaration(_)) | None => Ok(()),
                _ => Err(SourceLoweringError::unsupported(
                    "export_declaration_non_function",
                    stmt.span(),
                )),
            }
        }
        // §16.2.3 `export default …` — the outer wrapper is
        // pushed into `script_body` unchanged by `lower_program`
        // for every non-named-function shape. Dispatch by the
        // inner declaration kind:
        //
        // - Named class → same path as a top-level class decl;
        //   the class name is the export local.
        // - Named function → already registered as a regular
        //   top-level declaration; no-op at script time.
        // - Expression / anonymous → evaluate into acc and bind
        //   the result to `__otter_default` so the
        //   exported-const flush at the top-level tail installs
        //   it on the global object.
        Statement::ExportDefaultDeclaration(decl) => {
            ctx.record_source_location(builder.pc(), stmt.span().start);
            match &decl.declaration {
                ExportDefaultDeclarationKind::ClassDeclaration(cls) if cls.id.is_some() => {
                    lower_nested_class_declaration(builder, ctx, cls)
                }
                ExportDefaultDeclarationKind::FunctionDeclaration(func) if func.id.is_some() => {
                    let _ = func;
                    Ok(())
                }
                ExportDefaultDeclarationKind::ClassDeclaration(cls) => {
                    lower_class_expression(builder, ctx, cls)?;
                    lower_default_export_initializer(builder, ctx, stmt.span())
                }
                ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    lower_function_expression(builder, ctx, func)?;
                    lower_default_export_initializer(builder, ctx, stmt.span())
                }
                other => {
                    let expr = other.to_expression();
                    lower_return_expression(builder, ctx, expr)?;
                    lower_default_export_initializer(builder, ctx, stmt.span())
                }
            }
        }
        _ => lower_nested_statement(builder, ctx, stmt),
    }
}

/// Stores the current default-export value from acc into the
/// synthetic module-local binding used by anonymous default
/// declarations and default-export expressions.
///
/// Spec: https://tc39.es/ecma262/#sec-exports
fn lower_default_export_initializer<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    span: Span,
) -> Result<(), SourceLoweringError> {
    let slot = ctx.allocate_local(MODULE_DEFAULT_EXPORT_LOCAL, true, span)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (export default expr): {err:?}"))
        })?;
    ctx.mark_initialized(MODULE_DEFAULT_EXPORT_LOCAL)?;
    Ok(())
}

/// Lowers a single statement in a "nested" context (inside an `if`
/// branch, a `while` body, a `for` body, or a nested
/// `BlockStatement`). The accepted surface is a strict subset of
/// [`lower_top_statement`]: it does **not** allow `let`/`const`
/// declarations as a statement, since the compiler has no block
/// scoping and hoisting them to the surrounding function scope
/// would alter observable semantics. Inline `return` statements are
/// accepted (early-return pattern). `for (let …; …; …)` is special-
/// cased inside [`lower_for_statement`], which uses
/// [`LoweringContext::snapshot_scope`] / [`restore_scope`] to give
/// the for-init `let` a real lexical lifetime.
///
/// Takes `&mut ctx` so a `for` whose init is a `let` can call
/// `allocate_local` without an extra dispatch level.
pub(super) fn lower_nested_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    stmt: &'a Statement<'a>,
) -> Result<(), SourceLoweringError> {
    // D2: every statement starts at its AST span's byte offset.
    // Recording the PC about to be emitted (= current bytecode
    // length) → (line, column) gives the error reporter and
    // future debugger a precise anchor without touching any
    // expression-level helper. Finer granularity (per-opcode)
    // can layer on top of this later.
    ctx.record_source_location(builder.pc(), stmt.span().start);
    match stmt {
        Statement::ExpressionStatement(expr_stmt) => {
            // Statement-position expressions: lower any value-
            // producing expression and discard the accumulator on
            // return. The common shapes (AssignmentExpression,
            // CallExpression, UpdateExpression) still take the
            // direct path to avoid the extra indirection, but
            // `delete obj.x;`, `obj.x;` (bare member read —
            // triggers a getter), `void expr;`, etc. also work.
            match &expr_stmt.expression {
                Expression::AssignmentExpression(assign) => {
                    lower_assignment_expression(builder, ctx, assign)
                }
                Expression::CallExpression(call) => lower_call_expression(builder, ctx, call),
                Expression::UpdateExpression(update) => {
                    lower_update_expression(builder, ctx, update)
                }
                _ => lower_return_expression(builder, ctx, &expr_stmt.expression),
            }
        }
        Statement::IfStatement(if_stmt) => lower_if_statement(builder, ctx, if_stmt),
        Statement::WhileStatement(while_stmt) => lower_while_statement(builder, ctx, while_stmt),
        Statement::DoWhileStatement(do_stmt) => lower_do_while_statement(builder, ctx, do_stmt),
        Statement::ForStatement(for_stmt) => lower_for_statement(builder, ctx, for_stmt),
        Statement::ForOfStatement(for_of) => lower_for_of_statement(builder, ctx, for_of),
        Statement::ForInStatement(for_in) => lower_for_in_statement(builder, ctx, for_in),
        Statement::SwitchStatement(sw) => lower_switch_statement(builder, ctx, sw),
        Statement::FunctionDeclaration(func) => {
            lower_nested_function_declaration(builder, ctx, func)
        }
        Statement::ClassDeclaration(class) => lower_nested_class_declaration(builder, ctx, class),
        Statement::ThrowStatement(throw) => lower_throw_statement(builder, ctx, throw),
        Statement::TryStatement(try_stmt) => lower_try_statement(builder, ctx, try_stmt),
        Statement::BreakStatement(break_stmt) => lower_break_statement(builder, ctx, break_stmt),
        Statement::ContinueStatement(cont_stmt) => {
            lower_continue_statement(builder, ctx, cont_stmt)
        }
        Statement::ReturnStatement(ret) => lower_return_statement(builder, ctx, ret),
        Statement::BlockStatement(block) => lower_block_statement(builder, ctx, block),
        Statement::LabeledStatement(labeled) => lower_labeled_statement(builder, ctx, labeled),
        Statement::VariableDeclaration(decl) => match decl.kind {
            // `var` is a valid statement-position body for `if` /
            // `while` / `do-while` / labelled statements. We already
            // lower `var` through the shared declaration path as a
            // declaration-site local, so reuse that here instead of
            // keeping the stale blanket rejection.
            VariableDeclarationKind::Var => lower_let_const_declaration(builder, ctx, decl),
            _ => Err(SourceLoweringError::unsupported(
                "parser_recovery_bare_nested_lexical_declaration",
                decl.span,
            )),
        },
        other => Err(SourceLoweringError::unsupported(
            statement_construct_tag(other),
            other.span(),
        )),
    }
}

/// Lowers a `BlockStatement` with its own lexical scope (M12).
///
/// A fresh scope snapshot brackets the block body so any `let` /
/// `const` declared inside the block pops off the locals stack on
/// exit. Slot reservations survive via
/// [`LoweringContext::peak_local_count`], matching the `for`-init
/// scoping model — bindings that came in between enter and exit
/// keep their frame slots allocated, so a later sibling block can't
/// reuse them (which would be visibly wrong if a closure snapshotted
/// the old slot).
///
/// Nested blocks compose naturally: each block pushes its own
/// snapshot, and the popped-but-reserved slots stack in LIFO order.
/// `let`/`const` in an `if` / `while` / `for` body is accepted only
/// through a `{ ... }` wrapper per the JS spec (lexical declarations
/// in a bare Statement position are a SyntaxError the parser
/// already rejects).
///
/// Non-declaration statements inside the block fall through to
/// [`lower_nested_statement`] so the full nested-statement surface —
/// `if` / `while` / `for` / `return` / `break` / `continue` / inner
/// blocks / expression statements — keeps working unchanged.
pub(super) fn lower_block_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    block: &'a oxc_ast::ast::BlockStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let scope = ctx.snapshot_scope();
    let result = lower_nested_statement_list(builder, ctx, &block.body);
    ctx.restore_scope(scope);
    result
}

/// §14.13 `LabelName : Statement` — attaches a label to the
/// enclosed statement so `break labelName` / `continue labelName`
/// can target it.
///
/// - Iteration body (`for` / `while` / `do-while` / `for-of` /
///   `for-in`) or `switch`: the label is stashed on the context
///   via `set_pending_loop_label`; the nested lowerer consumes it
///   when it pushes its `LoopLabels` frame, so the stack stays a
///   single level deep.
/// - Anything else (a block, an expression statement, an `if`,
///   another labelled statement): a dedicated break-only frame
///   is pushed so `break labelName` from deep inside the body
///   jumps past the labelled statement. `continue labelName` in
///   that position is §14.11 invalid (no iteration target) and
///   reported as `undeclared_label`.
fn lower_labeled_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    labeled: &'a oxc_ast::ast::LabeledStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let name: std::rc::Rc<str> = std::rc::Rc::from(labeled.label.name.as_str());
    match &labeled.body {
        Statement::WhileStatement(_)
        | Statement::DoWhileStatement(_)
        | Statement::ForStatement(_)
        | Statement::ForOfStatement(_)
        | Statement::ForInStatement(_)
        | Statement::SwitchStatement(_) => {
            // Let the iteration / switch lowerer pick up the label.
            ctx.set_pending_loop_label(std::rc::Rc::clone(&name));
            lower_nested_statement(builder, ctx, &labeled.body)
        }
        _ => {
            // Break-only labelled statement — `break labelName`
            // jumps to the synthesized exit label, any other
            // control flow passes through.
            let break_label = builder.new_label();
            ctx.enter_loop(LoopLabels {
                break_label,
                continue_label: None,
                label: Some(std::rc::Rc::clone(&name)),
            });
            let result = lower_nested_statement(builder, ctx, &labeled.body);
            ctx.exit_loop();
            result?;
            builder.bind_label(break_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind labelled block exit: {err:?}"))
            })?;
            Ok(())
        }
    }
}

/// Lowers an `if (test) consequent` (with optional `else alternate`).
/// Bytecode shape:
///
/// ```text
/// without `else`:
///   <lower test>
///   JumpIfToBooleanFalse end_label
///   <lower consequent>
/// end_label:
///
/// with `else`:
///   <lower test>
///   JumpIfToBooleanFalse else_label
///   <lower consequent>
///   Jump end_label
/// else_label:
///   <lower alternate>
/// end_label:
/// ```
///
/// `JumpIfToBooleanFalse` performs JS truthy/falsy coercion so the
/// condition can be any value, not just a strict boolean — the
/// interpreter handles the `ToBoolean` step. Branches are lowered via
/// [`lower_nested_statement`] so they can themselves contain `if`s,
/// assignments, and inline `return`s.
fn lower_if_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    if_stmt: &'a oxc_ast::ast::IfStatement<'a>,
) -> Result<(), SourceLoweringError> {
    // Lower the condition into the accumulator. Reuses
    // `lower_return_expression` so any acc-producing expression
    // already supported (identifier, literal, binary, assignment,
    // parenthesised) works as a condition.
    lower_return_expression(builder, ctx, &if_stmt.test)?;

    let else_label = builder.new_label();
    let jmp_pc = builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, else_label)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanFalse: {err:?}"))
        })?;
    ctx.attach_branch_feedback(builder, jmp_pc);

    lower_nested_statement(builder, ctx, &if_stmt.consequent)?;

    if let Some(alternate) = &if_stmt.alternate {
        let end_label = builder.new_label();
        builder
            .emit_jump_to(Opcode::Jump, end_label)
            .map_err(|err| SourceLoweringError::Internal(format!("encode Jump: {err:?}")))?;
        builder
            .bind_label(else_label)
            .map_err(|err| SourceLoweringError::Internal(format!("bind else label: {err:?}")))?;
        lower_nested_statement(builder, ctx, alternate)?;
        builder
            .bind_label(end_label)
            .map_err(|err| SourceLoweringError::Internal(format!("bind end label: {err:?}")))?;
    } else {
        builder
            .bind_label(else_label)
            .map_err(|err| SourceLoweringError::Internal(format!("bind else label: {err:?}")))?;
    }

    Ok(())
}

/// Lowers a `while (test) body` statement. Bytecode shape:
///
/// ```text
/// loop_header:
///   <lower test>
///   JumpIfToBooleanFalse loop_exit
///   <lower body>
///   Jump loop_header
/// loop_exit:
/// ```
///
/// The `Jump loop_header` at the bottom is a backward branch — the
/// dispatcher's tier-up budget decrements on every backward jump, so
/// the loop body accrues hotness exactly the way the JIT expects.
/// `break` and `continue` (unlabelled) are supported via the
/// `LoopLabels` stack: `break` forward-jumps to `loop_exit`, and
/// `continue` backward-jumps to `loop_header`. Labelled jumps are
/// rejected. The body is lowered via [`lower_nested_statement`] so
/// it can contain assignments, nested `if`/`while`, blocks, and
/// inline `return`s — but no `let`/`const` (block scoping lands
/// later).
fn lower_while_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    while_stmt: &'a oxc_ast::ast::WhileStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let loop_header = builder.new_label();
    let loop_exit = builder.new_label();

    builder
        .bind_label(loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("bind loop header: {err:?}")))?;

    lower_return_expression(builder, ctx, &while_stmt.test)?;
    let jmp_pc = builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, loop_exit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanFalse: {err:?}"))
        })?;
    ctx.attach_branch_feedback(builder, jmp_pc);

    // Register this loop's jump targets so any nested `break` /
    // `continue` can find them. `while` uses the loop header as the
    // continue target — re-running the test is the spec-correct
    // semantics.
    ctx.enter_loop(LoopLabels {
        break_label: loop_exit,
        continue_label: Some(loop_header),
        label: ctx.take_pending_loop_label(),
    });
    let body_result = lower_nested_statement(builder, ctx, &while_stmt.body);
    ctx.exit_loop();
    body_result?;

    builder
        .emit_jump_to(Opcode::Jump, loop_header)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Jump (loop back): {err:?}"))
        })?;
    builder
        .bind_label(loop_exit)
        .map_err(|err| SourceLoweringError::Internal(format!("bind loop exit: {err:?}")))?;

    Ok(())
}

/// §14.7.2 `do { body } while (test)` — test runs *after* the body,
/// so the body always executes at least once. Bytecode shape:
///
/// ```text
/// loop_header:
///   <lower body>
/// continue_target:
///   <lower test>
///   JumpIfToBooleanTrue loop_header
/// loop_exit:
/// ```
///
/// `continue` jumps past the body to re-run the test (per spec),
/// `break` exits the loop entirely.
fn lower_do_while_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    do_stmt: &'a oxc_ast::ast::DoWhileStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let loop_header = builder.new_label();
    let continue_target = builder.new_label();
    let loop_exit = builder.new_label();

    builder
        .bind_label(loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("bind do-while header: {err:?}")))?;

    ctx.enter_loop(LoopLabels {
        break_label: loop_exit,
        continue_label: Some(continue_target),
        label: ctx.take_pending_loop_label(),
    });
    let body_result = lower_nested_statement(builder, ctx, &do_stmt.body);
    ctx.exit_loop();
    body_result?;

    builder
        .bind_label(continue_target)
        .map_err(|err| SourceLoweringError::Internal(format!("bind do-while continue: {err:?}")))?;
    lower_return_expression(builder, ctx, &do_stmt.test)?;
    let jmp_pc = builder
        .emit_jump_to(Opcode::JumpIfToBooleanTrue, loop_header)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanTrue (do-while): {err:?}"))
        })?;
    ctx.attach_branch_feedback(builder, jmp_pc);
    builder
        .bind_label(loop_exit)
        .map_err(|err| SourceLoweringError::Internal(format!("bind do-while exit: {err:?}")))?;

    Ok(())
}

/// Lowers a `for (init; test; update) body` statement. Bytecode shape:
///
/// ```text
///   <lower init>           ; let / const / assignment / nothing
/// loop_header:
///   <lower test>           ; or LdaTrue when omitted
///   JumpIfToBooleanFalse loop_exit
///   <lower body>
///   <lower update>         ; or no-op when omitted
///   Jump loop_header
/// loop_exit:
/// ```
///
/// Equivalent to the standard `for → while` desugaring:
///
/// ```text
///   { <init>; while (<test>) { <body>; <update>; } }
/// ```
///
/// `for (let i = …; …; …)` scopes the init binding to the loop —
/// uses [`LoweringContext::snapshot_scope`] / [`restore_scope`] to
/// pop the binding on loop exit while keeping the FrameLayout's
/// reservation in place. `for (;;)` is accepted; the body must
/// contain a `return` to terminate (no `break` yet). `for (… in …)`
/// and `for (… of …)` are separate AST node types and rejected with
/// their own tags.
fn lower_for_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    for_stmt: &'a oxc_ast::ast::ForStatement<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::ForStatementInit;

    if let Some(ForStatementInit::VariableDeclaration(decl)) = &for_stmt.init
        && matches!(
            decl.kind,
            VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
        )
    {
        return lower_classic_for_using_statement(builder, ctx, for_stmt, decl);
    }

    // Snapshot scope so any `let` introduced by the init pops on exit.
    let scope = ctx.snapshot_scope();

    // 1) Init.
    if let Some(init) = &for_stmt.init {
        match init {
            ForStatementInit::VariableDeclaration(decl) => {
                lower_let_const_declaration(builder, ctx, decl)?;
            }
            // `for (i = 0; …)` — init inherits the `Expression`
            // variants. Only an assignment expression makes sense at
            // statement-equivalent position; anything else (bare
            // read, call, comma) is rejected with a stable tag.
            ForStatementInit::AssignmentExpression(assign) => {
                lower_assignment_expression(builder, ctx, assign)?;
            }
            // Any other expression-shaped init (call, update,
            // sequence, etc.) — lower for side effects, discard
            // the accumulator. `ForStatementInit` inherits every
            // `Expression` variant via oxc's `inherit_variants!`
            // macro, so `to_expression()` gives us the borrowed
            // Expression to run through the regular lowerer.
            other => {
                lower_return_expression(builder, ctx, other.to_expression())?;
            }
        }
    }

    let loop_header = builder.new_label();
    let loop_exit = builder.new_label();
    // `continue` in a `for` jumps to the update clause (or the
    // loop header when there's no update). Using a dedicated
    // `loop_continue` label lets both paths share the bind sequence
    // below without leaking the difference to callers.
    let loop_continue = builder.new_label();

    builder
        .bind_label(loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("bind for header: {err:?}")))?;

    // 2) Test. Omitted test ⇒ unconditional loop, lowered as
    //    `LdaTrue` so the `JumpIfToBooleanFalse` path stays uniform
    //    with `while`. The interpreter / JIT can fold the constant-
    //    true branch later; emitting it now keeps the bytecode
    //    shape predictable for the v2 dispatcher.
    if let Some(test) = &for_stmt.test {
        lower_return_expression(builder, ctx, test)?;
    } else {
        builder
            .emit(Opcode::LdaTrue, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode LdaTrue: {err:?}")))?;
    }
    let jmp_pc = builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, loop_exit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanFalse: {err:?}"))
        })?;
    ctx.attach_branch_feedback(builder, jmp_pc);

    // 3) Body. Register the loop frame first so nested
    //    `break` / `continue` pick up our labels; pop after the
    //    body lowering completes.
    ctx.enter_loop(LoopLabels {
        break_label: loop_exit,
        continue_label: Some(loop_continue),
        label: ctx.take_pending_loop_label(),
    });
    let body_result = lower_nested_statement(builder, ctx, &for_stmt.body);
    ctx.exit_loop();
    body_result?;

    // 4) Continue target — runs the update clause (if any) and then
    //    falls through to the back-jump. `continue` from the body
    //    lands here, so the update still executes per spec.
    builder
        .bind_label(loop_continue)
        .map_err(|err| SourceLoweringError::Internal(format!("bind for continue: {err:?}")))?;

    // 5) Update — runs after every iteration, before the back-jump.
    //    M10 also accepts `UpdateExpression` (`i++` / `++i`),
    //    matching the canonical `for (let i = 0; i < n; i++)` idiom.
    //    The UpdateExpression's accumulator result is discarded.
    if let Some(update) = &for_stmt.update {
        match update {
            Expression::AssignmentExpression(assign) => {
                lower_assignment_expression(builder, ctx, assign)?;
            }
            Expression::UpdateExpression(update_expr) => {
                lower_update_expression(builder, ctx, update_expr)?;
            }
            Expression::CallExpression(call) => lower_call_expression(builder, ctx, call)?,
            // Any other expression in the update slot — lower and
            // discard. `for (let i = 0; i < n; log(i), i++)` uses
            // a SequenceExpression; `for (…; …; obj.method())` is
            // the CallExpression case already above.
            other => {
                lower_return_expression(builder, ctx, other)?;
            }
        }
    }

    builder
        .emit_jump_to(Opcode::Jump, loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("encode Jump (for back): {err:?}")))?;
    builder
        .bind_label(loop_exit)
        .map_err(|err| SourceLoweringError::Internal(format!("bind for exit: {err:?}")))?;

    ctx.restore_scope(scope);
    Ok(())
}

/// M30: lowers `for (<left> of <iterable>) <body>`.
///
/// Bytecode shape:
///
/// ```text
///   <lower iterable> → acc
///   Star r_src
///   GetIterator r_src → acc = iterator
///   Star r_iter
/// loop_top:                    ; also `continue` target
///   IteratorStep r_binding r_iter
///     ; writes done → acc, value → r_binding when not done
///   JumpIfToBooleanTrue loop_exit
///   <lower body>
///   Jump loop_top
/// loop_exit:
/// ```
///
/// Left-hand side forms supported in M30:
/// - `let x` / `const x` — fresh binding scoped to the loop body
///   (note: the M30 lowering reuses one slot per iteration;
///   spec-accurate CreatePerIterationEnvironment is a follow-up,
///   relevant only for body closures that capture the binding).
/// - plain `Identifier` target — assigns to an existing binding,
///   including a captured outer binding.
///
/// Deferred to later milestones: `for await`, destructuring
/// patterns in `left`, iterator-close on abrupt completion
/// (`break` / `return` through a custom iterator), async
/// iterators.
fn lower_for_of_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    for_of: &'a oxc_ast::ast::ForOfStatement<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::ForStatementLeft;
    use oxc_ast::ast::VariableDeclarationKind;
    if for_of.r#await {
        return Err(SourceLoweringError::unsupported(
            "for_await_of_statement",
            for_of.span,
        ));
    }

    // Snapshot scope so any `let` bindings introduced by `left`
    // pop on loop exit — mirrors how `for` init bindings work.
    let scope = ctx.snapshot_scope();

    // 1) Reserve iterator bookkeeping slots as hidden locals.
    //    Nested `for…of` loops shift `peak_local_count` upward as
    //    inner body bindings are allocated, so the iterator + src
    //    registers must live in the locals region rather than the
    //    temp region. Using `allocate_anonymous_local` keeps them
    //    safe from later `let`/`const` allocations inside the body.
    let src_local = ctx.allocate_anonymous_local()?;
    let iter_local = ctx.allocate_anonymous_local()?;
    let src_temp = src_local;
    let iter_temp = iter_local;

    let result = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &for_of.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (for-of iterable): {err:?}"))
            })?;
        builder
            .emit(Opcode::GetIterator, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode GetIterator: {err:?}")))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(iter_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (for-of iterator): {err:?}"))
            })?;

        // 2) Resolve the binding register. Three shapes:
        //    - `let x` / `const x`: allocate a fresh local.
        //    - `let [a, b]` / `let { x }`: allocate an anonymous
        //      local to hold each iteration's value; a
        //      destructuring pattern-bind runs before the body.
        //    - `x` (identifier assignment): reuse the existing
        //      binding's register, or spill through a hidden
        //      local before storing into an upvalue.
        let mut destructuring_pattern: Option<(&BindingPattern<'a>, bool)> = None;
        let mut assignment_target: Option<ForInOfAssignmentTarget<'a>> = None;
        let mut upvalue_target: Option<(u16, u16)> = None;
        let mut loop_using_await_dispose: Option<bool> = None;
        let (binding_reg, is_let_like) = match &for_of.left {
            ForStatementLeft::VariableDeclaration(decl) => {
                // `var`, `let`, and `const` all flow through the
                // same allocate-local + per-iteration store path
                // for the for-of target. `var` stays
                // block-scoped-like here until full function
                // hoisting lands — same compromise as plain
                // `var` declarations elsewhere.
                if decl.declarations.len() != 1 {
                    return Err(SourceLoweringError::unsupported(
                        "for_of_multiple_bindings",
                        decl.span,
                    ));
                }
                let declarator = &decl.declarations[0];
                if declarator.init.is_some() {
                    return Err(SourceLoweringError::unsupported(
                        "for_of_binding_initializer",
                        declarator.span,
                    ));
                }
                let is_using = matches!(
                    decl.kind,
                    VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
                );
                let is_const = decl.kind == VariableDeclarationKind::Const || is_using;
                if is_using {
                    loop_using_await_dispose =
                        Some(decl.kind == VariableDeclarationKind::AwaitUsing);
                }
                match &declarator.id {
                    oxc_ast::ast::BindingPattern::BindingIdentifier(ident) => {
                        let name = ident.name.as_str();
                        let slot = ctx.allocate_local(name, is_const, declarator.span)?;
                        ctx.mark_initialized(name)?;
                        (slot, true)
                    }
                    // Destructuring for-of target: allocate an
                    // anonymous hidden local to hold the per-
                    // iteration value, then run the pattern bind
                    // against it once we enter the body.
                    oxc_ast::ast::BindingPattern::ArrayPattern(_)
                    | oxc_ast::ast::BindingPattern::ObjectPattern(_)
                        if !is_using =>
                    {
                        let iter_val_slot = ctx.allocate_anonymous_local()?;
                        destructuring_pattern = Some((&declarator.id, is_const));
                        (iter_val_slot, true)
                    }
                    other => {
                        return Err(SourceLoweringError::unsupported(
                            if is_using {
                                "parser_recovery_for_of_using_pattern"
                            } else {
                                "for_of_destructuring_binding"
                            },
                            other.span(),
                        ));
                    }
                }
            }
            _ => match classify_for_in_of_left(&for_of.left, "parser_recovery_for_of_lhs")? {
                ForInOfLeft::Identifier(ident) => {
                    let name = ident.name.as_str();
                    let binding = ctx.resolve_identifier(name).ok_or_else(|| {
                        SourceLoweringError::unsupported("unbound_identifier", ident.span)
                    })?;
                    match binding {
                        BindingRef::Local {
                            reg,
                            initialized: true,
                            is_const: false,
                            ..
                        } => (reg, false),
                        BindingRef::Param { reg } => (reg, false),
                        BindingRef::Local { is_const: true, .. } => {
                            return Err(SourceLoweringError::unsupported(
                                "const_assignment",
                                ident.span,
                            ));
                        }
                        BindingRef::Local {
                            initialized: false, ..
                        } => {
                            return Err(SourceLoweringError::unsupported(
                                "tdz_self_reference",
                                ident.span,
                            ));
                        }
                        BindingRef::Upvalue {
                            idx,
                            is_const: false,
                        } => {
                            let iter_val_slot = ctx.allocate_anonymous_local()?;
                            upvalue_target = Some((iter_val_slot, idx));
                            (iter_val_slot, false)
                        }
                        BindingRef::Upvalue { is_const: true, .. } => {
                            return Err(SourceLoweringError::unsupported(
                                "const_assignment",
                                ident.span,
                            ));
                        }
                    }
                }
                ForInOfLeft::AssignmentTarget(target) => {
                    let iter_val_slot = ctx.allocate_anonymous_local()?;
                    assignment_target = Some(target);
                    (iter_val_slot, false)
                }
            },
        };
        let _ = is_let_like;

        // 3) Loop skeleton.
        let loop_top = builder.new_label();
        let loop_exit = builder.new_label();
        builder
            .bind_label(loop_top)
            .map_err(|err| SourceLoweringError::Internal(format!("bind for-of top: {err:?}")))?;
        builder
            .emit(
                Opcode::IteratorStep,
                &[
                    Operand::Reg(u32::from(binding_reg)),
                    Operand::Reg(u32::from(iter_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode IteratorStep: {err:?}"))
            })?;
        let jmp_pc = builder
            .emit_jump_to(Opcode::JumpIfToBooleanTrue, loop_exit)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfToBooleanTrue (for-of done): {err:?}"
                ))
            })?;
        ctx.attach_branch_feedback(builder, jmp_pc);
        if let Some((iter_val_reg, upvalue_idx)) = upvalue_target {
            lower_for_in_of_upvalue_assignment(builder, iter_val_reg, upvalue_idx)?;
        }
        if let Some(target) = assignment_target {
            lower_for_in_of_assignment_target(builder, ctx, target, binding_reg, true)?;
        }

        // 4) Body. Register loop labels so nested
        //    `break` / `continue` target our skeleton — `continue`
        //    resumes at the iterator-step, `break` jumps past
        //    the loop.
        ctx.enter_loop(LoopLabels {
            break_label: loop_exit,
            continue_label: Some(loop_top),
            label: ctx.take_pending_loop_label(),
        });
        let body_result = if let Some(await_dispose) = loop_using_await_dispose {
            lower_loop_using_iteration(builder, ctx, binding_reg, await_dispose, |builder, ctx| {
                lower_nested_statement(builder, ctx, &for_of.body)
            })
        } else {
            (|| -> Result<(), SourceLoweringError> {
                // Destructuring for-of: expand the pattern against
                // the iterator value now in `binding_reg` so every
                // leaf becomes a fresh per-iteration local.
                if let Some((pattern, is_const)) = destructuring_pattern {
                    lower_pattern_bind(builder, ctx, pattern, binding_reg, is_const)?;
                }
                lower_nested_statement(builder, ctx, &for_of.body)
            })()
        };
        ctx.exit_loop();
        body_result?;

        builder
            .emit_jump_to(Opcode::Jump, loop_top)
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Jump (for-of back): {err:?}"))
            })?;
        builder
            .bind_label(loop_exit)
            .map_err(|err| SourceLoweringError::Internal(format!("bind for-of exit: {err:?}")))?;
        Ok(())
    })();

    ctx.restore_scope(scope);
    result
}

/// Performs ForIn/OfBodyEvaluation's assignment step for
/// `for (x of iterable)` / `for (x in object)` when `x` resolves
/// to an upvalue.
///
/// Spec: https://tc39.es/ecma262/#sec-runtime-semantics-forin-div-ofbodyevaluation-lhs-stmt-iterator-lhskind-labelset
fn lower_for_in_of_upvalue_assignment(
    builder: &mut BytecodeBuilder,
    iter_value_reg: u16,
    upvalue_idx: u16,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(iter_value_reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Ldar (for-of upvalue target): {err:?}"))
        })?;
    builder
        .emit(Opcode::StaUpvalue, &[Operand::Idx(u32::from(upvalue_idx))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode StaUpvalue (for-of upvalue target): {err:?}"
            ))
        })?;
    Ok(())
}

/// M31: lowers `for (<left> in <source>) <body>` — §14.7.5.11
/// ForInOfStatement, `in` variant. Walks the source's own +
/// inherited enumerable string-keyed property names via the
/// runtime's property iterator (allocated by `ForInEnumerate`,
/// stepped by `ForInNext`).
///
/// Bytecode shape:
///
/// ```text
///   <lower source> → acc
///   Star r_src
///   ForInEnumerate r_src → acc = property_iterator
///   Star r_iter
/// loop_top:
///   ForInNext r_binding r_iter
///     ; writes done → acc, key → r_binding when not done
///   JumpIfToBooleanTrue loop_exit
///   <lower body>
///   Jump loop_top
/// loop_exit:
/// ```
///
/// `null` / `undefined` sources don't throw — `ForInEnumerate`
/// allocates an empty iterator per §14.7.5.6 step 6, so the
/// body never runs.
///
/// Supported LHS forms mirror `for…of`: `let x` / `const x`
/// (fresh per-loop binding) and plain identifier targets,
/// including captured outer bindings. Same deferrals apply
/// (destructuring assignment targets, `var` hoisting details).
fn lower_for_in_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    for_in: &'a oxc_ast::ast::ForInStatement<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::ForStatementLeft;
    use oxc_ast::ast::VariableDeclarationKind;

    // Snapshot scope so any `let` bindings introduced by `left`
    // pop on loop exit.
    let scope = ctx.snapshot_scope();

    // Reserve iterator bookkeeping slots as hidden locals (same
    // reasoning as `for…of` — nested loops shift the temp base
    // and would clobber temp-region temps).
    let src_local = ctx.allocate_anonymous_local()?;
    let iter_local = ctx.allocate_anonymous_local()?;

    let result = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &for_in.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(src_local))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (for-in source): {err:?}"))
            })?;
        builder
            .emit(
                Opcode::ForInEnumerate,
                &[Operand::Reg(u32::from(src_local))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode ForInEnumerate: {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(iter_local))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (for-in iterator): {err:?}"))
            })?;

        let mut for_in_destructuring_pattern: Option<(&BindingPattern<'a>, bool)> = None;
        let mut assignment_target: Option<ForInOfAssignmentTarget<'a>> = None;
        let mut upvalue_target: Option<(u16, u16)> = None;
        let binding_reg = match &for_in.left {
            ForStatementLeft::VariableDeclaration(decl) => {
                // `var`, `let`, `const` all allocate the same
                // per-loop local; function-scope hoisting for the
                // `var` flavour is still tracked as a follow-up.
                if decl.declarations.len() != 1 {
                    return Err(SourceLoweringError::unsupported(
                        "for_in_multiple_bindings",
                        decl.span,
                    ));
                }
                let declarator = &decl.declarations[0];
                if declarator.init.is_some() {
                    return Err(SourceLoweringError::unsupported(
                        "for_in_binding_initializer",
                        declarator.span,
                    ));
                }
                let is_const = decl.kind == VariableDeclarationKind::Const;
                match &declarator.id {
                    oxc_ast::ast::BindingPattern::BindingIdentifier(ident) => {
                        let name = ident.name.as_str();
                        let slot = ctx.allocate_local(name, is_const, declarator.span)?;
                        ctx.mark_initialized(name)?;
                        slot
                    }
                    oxc_ast::ast::BindingPattern::ArrayPattern(_)
                    | oxc_ast::ast::BindingPattern::ObjectPattern(_) => {
                        // `for (const { k } in obj)` — stash the
                        // per-iteration KEY in an anon local, run
                        // the destructure against it at the top of
                        // the body. For-in keys are strings, so
                        // destructuring is unusual but still valid.
                        let iter_val_slot = ctx.allocate_anonymous_local()?;
                        for_in_destructuring_pattern = Some((&declarator.id, is_const));
                        iter_val_slot
                    }
                    _ => {
                        return Err(SourceLoweringError::unsupported(
                            "for_in_destructuring_binding",
                            declarator.span,
                        ));
                    }
                }
            }
            _ => match classify_for_in_of_left(&for_in.left, "parser_recovery_for_in_lhs")? {
                ForInOfLeft::Identifier(ident) => {
                    let name = ident.name.as_str();
                    let binding = ctx.resolve_identifier(name).ok_or_else(|| {
                        SourceLoweringError::unsupported("unbound_identifier", ident.span)
                    })?;
                    match binding {
                        BindingRef::Local {
                            reg,
                            initialized: true,
                            is_const: false,
                            ..
                        } => reg,
                        BindingRef::Param { reg } => reg,
                        BindingRef::Local { is_const: true, .. } => {
                            return Err(SourceLoweringError::unsupported(
                                "const_assignment",
                                ident.span,
                            ));
                        }
                        BindingRef::Local {
                            initialized: false, ..
                        } => {
                            return Err(SourceLoweringError::unsupported(
                                "tdz_self_reference",
                                ident.span,
                            ));
                        }
                        BindingRef::Upvalue {
                            idx,
                            is_const: false,
                        } => {
                            let iter_val_slot = ctx.allocate_anonymous_local()?;
                            upvalue_target = Some((iter_val_slot, idx));
                            iter_val_slot
                        }
                        BindingRef::Upvalue { is_const: true, .. } => {
                            return Err(SourceLoweringError::unsupported(
                                "const_assignment",
                                ident.span,
                            ));
                        }
                    }
                }
                ForInOfLeft::AssignmentTarget(target) => {
                    let iter_val_slot = ctx.allocate_anonymous_local()?;
                    assignment_target = Some(target);
                    iter_val_slot
                }
            },
        };

        let loop_top = builder.new_label();
        let loop_exit = builder.new_label();
        builder
            .bind_label(loop_top)
            .map_err(|err| SourceLoweringError::Internal(format!("bind for-in top: {err:?}")))?;
        builder
            .emit(
                Opcode::ForInNext,
                &[
                    Operand::Reg(u32::from(binding_reg)),
                    Operand::Reg(u32::from(iter_local)),
                ],
            )
            .map_err(|err| SourceLoweringError::Internal(format!("encode ForInNext: {err:?}")))?;
        let jmp_pc = builder
            .emit_jump_to(Opcode::JumpIfToBooleanTrue, loop_exit)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfToBooleanTrue (for-in done): {err:?}"
                ))
            })?;
        ctx.attach_branch_feedback(builder, jmp_pc);
        if let Some((iter_val_reg, upvalue_idx)) = upvalue_target {
            lower_for_in_of_upvalue_assignment(builder, iter_val_reg, upvalue_idx)?;
        }
        if let Some(target) = assignment_target {
            lower_for_in_of_assignment_target(builder, ctx, target, binding_reg, false)?;
        }

        ctx.enter_loop(LoopLabels {
            break_label: loop_exit,
            continue_label: Some(loop_top),
            label: ctx.take_pending_loop_label(),
        });
        let body_result = (|| -> Result<(), SourceLoweringError> {
            if let Some((pattern, is_const)) = for_in_destructuring_pattern {
                lower_pattern_bind(builder, ctx, pattern, binding_reg, is_const)?;
            }
            lower_nested_statement(builder, ctx, &for_in.body)
        })();
        ctx.exit_loop();
        body_result?;

        builder
            .emit_jump_to(Opcode::Jump, loop_top)
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Jump (for-in back): {err:?}"))
            })?;
        builder
            .bind_label(loop_exit)
            .map_err(|err| SourceLoweringError::Internal(format!("bind for-in exit: {err:?}")))?;
        Ok(())
    })();

    ctx.restore_scope(scope);
    result
}

/// Lowers `switch (e) { case v: …; default: …; }`. Bytecode shape:
///
/// ```text
///   <lower discriminant into acc>
///   Star r_disc                        ; r_disc = discriminant
///   ; Compare phase — one dispatch per case, in source order.
///   Ldar r_disc                        ; acc = discriminant
///   TestEqualStrict r_v0               ; acc = (discriminant === v0)
///   JumpIfToBooleanTrue case_0
///   Ldar r_disc
///   TestEqualStrict r_v1
///   JumpIfToBooleanTrue case_1
///   …
///   Jump default_label                 ; or `switch_exit` if no default
///   ; Body phase — labels sit above each case's statements, in source
///   ; order, so fall-through between cases works naturally. `break`
///   ; inside a case targets `switch_exit`.
/// case_0:
///   <lower case 0 consequent>
/// case_1:
///   <lower case 1 consequent>
///   …
/// default_label:
///   <lower default consequent>
/// switch_exit:
/// ```
///
/// Each case-value expression is lowered into acc and spilled into
/// its own temp before the compare phase — this keeps the
/// discriminant fresh in `r_disc` across comparisons and lets the
/// `TestEqualStrict` opcode read `acc = discriminant` and
/// `r_value` directly without extra reloads.
///
/// §14.11 SwitchStatement — `break` exits the switch; `continue`
/// walks past the switch to the enclosing loop.
///
/// Intentionally simple: no jump-table optimisation for dense
/// int32 cases, no deduplication of duplicate case values. Those
/// are JIT-level tricks that land when the bytecode surface
/// stabilises.
fn lower_switch_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    sw: &'a oxc_ast::ast::SwitchStatement<'a>,
) -> Result<(), SourceLoweringError> {
    hoist_switch_var_declarations(builder, ctx, sw)?;
    let switch_scope = enter_switch_lexical_scope(builder, ctx, sw)?;
    // 1) Evaluate discriminant into a temp. The compare phase
    //    reloads it before each `TestEqualStrict` so the acc is
    //    predictable when entering the comparison opcode.
    let disc_temp = ctx.acquire_temps(1)?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &sw.discriminant)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(disc_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (switch discriminant): {err:?}"))
            })?;

        // 2) Lower each `case <v>:` value into its own temp. We
        //    do this eagerly so the comparisons below can just
        //    `TestEqualStrict r_vN` without any re-evaluation.
        //    `default:` (test == None) doesn't consume a temp.
        let case_count = sw.cases.len();
        // Per-case labels — bound later above each case's body.
        let case_labels: Vec<Label> = (0..case_count).map(|_| builder.new_label()).collect();
        let switch_exit = builder.new_label();

        // Compute how many non-default cases we have so we can
        // acquire exactly that many value-temps.
        let value_case_count: u16 = sw
            .cases
            .iter()
            .filter(|c| c.test.is_some())
            .count()
            .try_into()
            .map_err(|_| SourceLoweringError::Internal("switch case count exceeds u16".into()))?;
        let value_base = if value_case_count == 0 {
            0
        } else {
            ctx.acquire_temps(value_case_count)?
        };

        let body_result = (|| -> Result<(), SourceLoweringError> {
            // Lower case values into consecutive temps. Index into
            // `value_base` advances only for non-default cases.
            let mut value_slot: u16 = 0;
            for case in sw.cases.iter() {
                let Some(test) = case.test.as_ref() else {
                    continue; // default — no value to evaluate.
                };
                lower_return_expression(builder, ctx, test)?;
                let slot = value_base.checked_add(value_slot).ok_or_else(|| {
                    SourceLoweringError::Internal("switch case value slot overflow".into())
                })?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (switch case value): {err:?}"
                        ))
                    })?;
                value_slot = value_slot
                    .checked_add(1)
                    .ok_or_else(|| SourceLoweringError::Internal("value_slot overflow".into()))?;
            }

            // 3) Compare phase. For each case with a test, emit
            //    `Ldar r_disc; TestEqualStrict r_vN;
            //    JumpIfToBooleanTrue case_label`. Default cases
            //    are skipped here and covered by the "no-match"
            //    fallback jump below.
            let mut value_slot: u16 = 0;
            let mut default_index: Option<usize> = None;
            for (case_idx, case) in sw.cases.iter().enumerate() {
                let Some(_test) = case.test.as_ref() else {
                    default_index = Some(case_idx);
                    continue;
                };
                builder
                    .emit(Opcode::Ldar, &[Operand::Reg(u32::from(disc_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Ldar (switch disc reload): {err:?}"
                        ))
                    })?;
                let value_reg = value_base.checked_add(value_slot).ok_or_else(|| {
                    SourceLoweringError::Internal("switch value reg overflow".into())
                })?;
                let cmp_pc = builder
                    .emit(
                        Opcode::TestEqualStrict,
                        &[Operand::Reg(u32::from(value_reg))],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode TestEqualStrict (switch): {err:?}"
                        ))
                    })?;
                ctx.attach_comparison_feedback(builder, cmp_pc);
                let jump_pc = builder
                    .emit_jump_to(Opcode::JumpIfToBooleanTrue, case_labels[case_idx])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode JumpIfToBooleanTrue (switch): {err:?}"
                        ))
                    })?;
                ctx.attach_branch_feedback(builder, jump_pc);
                value_slot = value_slot
                    .checked_add(1)
                    .ok_or_else(|| SourceLoweringError::Internal("value_slot overflow".into()))?;
            }

            // 4) No case matched — jump to `default` if present,
            //    otherwise skip the entire body to `switch_exit`.
            let fallback = match default_index {
                Some(idx) => case_labels[idx],
                None => switch_exit,
            };
            builder
                .emit_jump_to(Opcode::Jump, fallback)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Jump (switch fallback): {err:?}"))
                })?;

            // 5) Body phase. `enter_loop` pushes the break-only
            //    frame so any nested `break` in a case jumps to
            //    `switch_exit`; `continue` walks past this frame
            //    because `continue_label` is `None`.
            ctx.enter_loop(LoopLabels {
                break_label: switch_exit,
                continue_label: None,
                label: ctx.take_pending_loop_label(),
            });

            let lower_cases = (|| -> Result<(), SourceLoweringError> {
                for (case_idx, case) in sw.cases.iter().enumerate() {
                    builder.bind_label(case_labels[case_idx]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "bind switch case {case_idx}: {err:?}"
                        ))
                    })?;
                    for stmt in case.consequent.iter() {
                        lower_switch_case_statement(builder, ctx, stmt)?;
                    }
                }
                Ok(())
            })();
            ctx.exit_loop();
            lower_cases?;

            // 6) Exit label — bound after all case bodies so fall
            //    through to the bottom is a natural next instruction.
            builder.bind_label(switch_exit).map_err(|err| {
                SourceLoweringError::Internal(format!("bind switch exit: {err:?}"))
            })?;
            Ok(())
        })();
        if value_case_count > 0 {
            ctx.release_temps(value_case_count);
        }
        body_result
    })();
    ctx.release_temps(1); // disc_temp
    ctx.restore_scope(switch_scope);
    lower
}

/// Lowers `throw <expr>;`. Evaluates the argument into acc, emits
/// `Opcode::Throw`, and lets the interpreter's throw-transfer path
/// find the nearest enclosing handler in the function's
/// `ExceptionTable`.
///
/// §14.14 ThrowStatement.
fn lower_throw_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    throw: &'a oxc_ast::ast::ThrowStatement<'a>,
) -> Result<(), SourceLoweringError> {
    lower_return_expression(builder, ctx, &throw.argument)?;
    builder
        .emit(Opcode::Throw, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Throw: {err:?}")))?;
    Ok(())
}
