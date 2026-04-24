//! Function lowering: parameter analysis, body codegen, inner-
//! function emission, function / arrow / nested-declaration
//! expressions, and the `new` / `new ...spread` construct paths.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. Public entry points (all `pub(super)`):
//!
//! - Parameters & body: `ParamsLayout` (with its public fields so
//!   `classes` can synthesise zero-param frames),
//!   `feedback_layout_from_kinds`, `FunctionBodyOutput`,
//!   `analyze_params`, `lower_function_body`,
//!   `lower_function_body_with_parent`.
//! - Expression surface: `lower_function_expression`,
//!   `lower_arrow_function_expression`,
//!   `lower_nested_function_declaration`, `lower_new_expression`.
//! - Inner callable emitters: `lower_inner_function_with_captures`,
//!   `lower_inner_callable`, `lower_inner_callable_with_super` — the
//!   last is called directly by `classes::lower_class_body_core`.

use super::*;

/// Output of [`lower_function_body`]. Groups the bytecode with the
/// per-function side-table counts the caller wires into the
/// `Function`.
pub(super) struct FunctionBodyOutput {
    pub(super) bytecode: Bytecode,
    pub(super) local_count: RegisterIndex,
    pub(super) temp_count: RegisterIndex,
    pub(super) feedback_slot_count: u16,
    /// P1: per-slot feedback kinds, in allocation order. Used to
    /// build a heterogeneous `FeedbackTableLayout` — arithmetic
    /// feedback alongside property inline-cache feedback, call
    /// target feedback, etc.
    pub(super) feedback_slot_kinds: Vec<FeedbackKind>,
    pub(super) property_names: crate::property::PropertyNameTable,
    pub(super) float_constants: crate::float::FloatTable,
    pub(super) string_literals: crate::string::StringTable,
    pub(super) bigint_constants: crate::bigint::BigIntTable,
    pub(super) regexp_literals: crate::regexp::RegExpTable,
    pub(super) exceptions: crate::exception::ExceptionTable,
    pub(super) closures: crate::closure::ClosureTable,
    /// D2: `pc → (line, column)` map built from statement-level
    /// recordings during lowering. Empty when the compilation
    /// wasn't fed a source-text index (synthesised functions,
    /// test harnesses constructing modules manually).
    pub(super) source_map: crate::source_map::SourceMap,
}

/// Build a `FeedbackTableLayout` matching the kinds observed by the
/// lowering context. Source-compiled functions allocate slots in
/// monotonically increasing order, so mapping index → (slot id, kind)
/// lines up with the slot ids produced by
/// `LoweringContext::allocate_*_feedback`.
pub(super) fn feedback_layout_from_kinds(kinds: &[FeedbackKind]) -> FeedbackTableLayout {
    let slots: Vec<FeedbackSlotLayout> = kinds
        .iter()
        .enumerate()
        .map(|(i, k)| {
            FeedbackSlotLayout::new(FeedbackSlotId(u16::try_from(i).unwrap_or(u16::MAX)), *k)
        })
        .collect();
    FeedbackTableLayout::new(slots)
}

/// Structured result of `analyze_params`. Captures what the body
/// lowerer needs to emit correct parameter-setup bytecode at
/// function entry.
///
/// - `names[i]` — identifier name of the i-th non-rest parameter.
/// - `defaults[i]` — `Some(expr)` when the i-th param has a
///   default initializer; `None` otherwise.
/// - `rest_name` — `Some(name)` when the function has a rest
///   parameter (`function f(..., ...rest)`); `None` otherwise.
///
/// The rest parameter lives in a dedicated local slot (allocated
/// at body-lowering time), **not** in the parameter slot window —
/// the runtime's `CallDirect` / `CallProperty` paths copy only
/// non-rest arguments into parameter slots, with anything beyond
/// that count stashed in `activation.overflow_args` for the
/// `CreateRestParameters` opcode at function entry to pull into an
/// array.
pub(super) struct ParamsLayout<'a> {
    pub(super) names: Vec<&'a str>,
    pub(super) defaults: Vec<Option<&'a Expression<'a>>>,
    /// Per-param destructuring pattern. `Some(&pat)` means the
    /// param occupies a slot reserved for the raw argument value,
    /// and `emit_param_destructuring` must bind the pattern's
    /// leaves to fresh locals after the default-initializer pass.
    /// `None` means the param is a plain identifier at slot `i`
    /// and `names[i]` is the user-facing binding.
    pub(super) patterns: Vec<Option<&'a BindingPattern<'a>>>,
    pub(super) rest_name: Option<&'a str>,
    /// `function f(...[a, b])` — destructuring rest parameter.
    /// When set, the rest array still lands in an anonymous
    /// local; `emit_rest_parameter` then runs a pattern-bind
    /// against it to populate the leaf identifiers.
    pub(super) rest_pattern: Option<&'a BindingPattern<'a>>,
}

impl ParamsLayout<'_> {
    /// Count of actual parameter slots the FrameLayout reserves —
    /// one per non-rest param (the rest binding is a local, not a
    /// param slot).
    pub(super) fn param_slot_count(&self) -> RegisterIndex {
        RegisterIndex::try_from(self.names.len()).unwrap_or(u16::MAX)
    }
}

/// Walks a `FormalParameters` list, validates every param shape we
/// support at M22 (plain identifier patterns, optional default
/// initializer, optional single rest parameter), and produces a
/// `ParamsLayout` the body lowerer can drive off of.
///
/// Accepted shapes (per-param):
/// - `name` — plain identifier.
/// - `name = <expr>` — identifier with default initializer.
///
/// Accepted rest shape:
/// - `...rest` — plain identifier. No default allowed on rest
///   (spec forbids it anyway).
///
/// Parser-recovery guards:
/// - `parser_recovery_formal_param_assignment` — oxc documents
///   top-level `AssignmentPattern` as invalid in `FormalParameter`;
///   real parameter defaults arrive through `param.initializer`.
/// - `parser_recovery_rest_parameter_pattern` — rest initializers
///   are syntax errors before lowering; identifier and
///   destructuring rest patterns are first-class surfaces.
pub(super) fn analyze_params<'a>(
    params: &'a FormalParameters<'a>,
) -> Result<ParamsLayout<'a>, SourceLoweringError> {
    let mut names = Vec::with_capacity(params.items.len());
    let mut defaults = Vec::with_capacity(params.items.len());
    let mut patterns = Vec::with_capacity(params.items.len());

    for param in params.items.iter() {
        match &param.pattern {
            BindingPattern::BindingIdentifier(ident) => {
                names.push(ident.name.as_str());
                defaults.push(param.initializer.as_deref());
                patterns.push(None);
            }
            // M24: array / object destructuring parameter. The
            // param slot is synthesized (user code can't reach it
            // — `@p` isn't a legal JS identifier), and
            // `emit_param_destructuring` binds the pattern's
            // leaves to fresh locals after the default-init pass.
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                names.push("@p");
                defaults.push(param.initializer.as_deref());
                patterns.push(Some(&param.pattern));
            }
            // `function f(x = 5)` comes through the `BindingIdentifier`
            // path above — oxc flattens the default into
            // `param.initializer`, not into an AssignmentPattern.
            // AssignmentPattern at this level is parser recovery:
            // real defaults are carried by `param.initializer`.
            BindingPattern::AssignmentPattern(pat) => {
                return Err(SourceLoweringError::unsupported(
                    "parser_recovery_formal_param_assignment",
                    pat.span,
                ));
            }
        }
    }

    // Optional rest parameter. oxc wraps `...rest` in
    // `FormalParameters.rest: FormalParameterRest`, which itself
    // contains a `BindingRestElement { argument: BindingPattern }`.
    // Supports identifier rest (`function f(...rest)`) and
    // destructuring rest (`function f(...[a, b])` / `...{ a }`).
    let (rest_name, rest_pattern) = match params.rest.as_deref() {
        Some(rest) => match &rest.rest.argument {
            BindingPattern::BindingIdentifier(ident) => (Some(ident.name.as_str()), None),
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                (None, Some(&rest.rest.argument))
            }
            _ => {
                return Err(SourceLoweringError::unsupported(
                    "parser_recovery_rest_parameter_pattern",
                    rest.rest.span,
                ));
            }
        },
        None => (None, None),
    };

    Ok(ParamsLayout {
        names,
        defaults,
        patterns,
        rest_name,
        rest_pattern,
    })
}

/// Emits per-parameter default-initializer bytecode at function
/// entry, in declaration order. For each param with `default = Some(expr)`:
///
/// ```text
///   Ldar r_param                ; acc = caller-supplied arg (or undefined)
///   JumpIfNotUndefined skip
///   <lower default expr>         ; acc = default value
///   Star r_param
/// skip:
/// ```
///
/// Spec: §10.2.1 FunctionDeclarationInstantiation — defaults only
/// evaluate when the parameter binding is `undefined`, matching
/// both "caller omitted the argument" and "caller passed explicit
/// `undefined`".
///
/// Later defaults can reference earlier params (`f(a, b = a + 1)`)
/// because the iteration is in source order and each default
/// `Star`s into the param slot before the next default runs.
fn emit_default_initializers<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    layout: &ParamsLayout<'a>,
) -> Result<(), SourceLoweringError> {
    for (i, default) in layout.defaults.iter().enumerate() {
        let Some(expr) = default else { continue };
        let reg = u32::try_from(i)
            .map_err(|_| SourceLoweringError::Internal("param index overflow".into()))?;
        let skip = builder.new_label();
        // Ldar reads the param slot into acc. We intentionally
        // skip the feedback-slot attachment that
        // `lower_identifier_read` would add — this is a one-shot
        // prologue read, and polluting the feedback vector with
        // it would mark every default as `Any` for no gain.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(reg)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (default init): {err:?}"))
            })?;
        let jmp_pc = builder
            .emit_jump_to(Opcode::JumpIfNotUndefined, skip)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfNotUndefined (default): {err:?}"
                ))
            })?;
        ctx.attach_branch_feedback(builder, jmp_pc);
        // Lower default expression into acc, then spill.
        lower_return_expression(builder, ctx, expr)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(reg)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (default init): {err:?}"))
            })?;
        builder
            .bind_label(skip)
            .map_err(|err| SourceLoweringError::Internal(format!("bind default skip: {err:?}")))?;
    }
    Ok(())
}

/// For each destructuring parameter (array or object pattern),
/// emits the binding code that extracts leaves from the synthetic
/// param slot into fresh locals. Runs after
/// `emit_default_initializers` so `{ a = 1 }` per-leaf defaults
/// see the post-default param value.
///
/// Mirrors the `let` destructuring lowering — same
/// `lower_pattern_bind` helper, different "source register"
/// (the param slot, not a hidden local).
fn emit_param_destructuring<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    layout: &ParamsLayout<'a>,
) -> Result<(), SourceLoweringError> {
    for (i, pattern) in layout.patterns.iter().enumerate() {
        let Some(pat) = pattern else { continue };
        let param_reg = RegisterIndex::try_from(i)
            .map_err(|_| SourceLoweringError::Internal("param index overflow".into()))?;
        // Params are ordinary writable bindings (M22), so we pass
        // `is_const: false` — matches the spec's Mutable binding
        // kind for destructuring-param-introduced names.
        lower_pattern_bind(builder, ctx, pat, param_reg, false)?;
    }
    Ok(())
}

/// Materialises the rest parameter's array from
/// `activation.overflow_args` and binds it to a newly-allocated
/// local slot. Called at function entry after default
/// initializers.
///
/// `function f(a, b, ...rest)` — the runtime's `CallDirect` /
/// `CallProperty` copy only the non-rest args into parameter slots
/// (`param_count = 2` here); any additional arguments land in the
/// activation's `overflow_args`. `CreateRestParameters` drains
/// that into a fresh Array, which we then `Star` into `r_rest`.
///
/// The rest binding is a local (not a param slot) so it stays out
/// of the FrameLayout's `parameter_count` — that count matches the
/// runtime's arg-copy window.
fn emit_rest_parameter<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    layout: &ParamsLayout<'a>,
) -> Result<(), SourceLoweringError> {
    // Named rest — the simple `function f(...rest)` case.
    if let Some(rest_name) = layout.rest_name {
        // Allocate rest as a `const`-like local. ES spec treats
        // rest as a fresh binding (not a param alias).
        let slot = ctx.allocate_local(rest_name, true, Span::default())?;
        builder
            .emit(Opcode::CreateRestParameters, &[])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CreateRestParameters: {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode Star (rest): {err:?}")))?;
        ctx.mark_initialized(rest_name)?;
        return Ok(());
    }
    // Destructuring rest — `function f(...[a, b])` / `...{ a }`.
    // Build the rest array into an anonymous local, then let the
    // shared pattern-bind helper expand the pattern's leaves into
    // fresh user-visible locals.
    if let Some(pattern) = layout.rest_pattern {
        let slot = ctx.allocate_anonymous_local()?;
        builder
            .emit(Opcode::CreateRestParameters, &[])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode CreateRestParameters (destruct): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (destruct rest): {err:?}"))
            })?;
        lower_pattern_bind(builder, ctx, pattern, slot, true)?;
    }
    Ok(())
}

pub(super) fn lower_function_body<'a>(
    body: &'a FunctionBody<'a>,
    params: &'a FormalParameters<'a>,
    layout: &ParamsLayout<'a>,
    function_names: &'a [&'a str],
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
) -> Result<FunctionBodyOutput, SourceLoweringError> {
    lower_function_body_with_parent(
        body,
        params,
        layout,
        function_names,
        module_functions,
        None,
        None,
        None,
    )
    .map(|(out, _captures)| out)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn lower_function_body_with_parent<'a>(
    body: &'a FunctionBody<'a>,
    _params: &'a FormalParameters<'a>,
    layout: &ParamsLayout<'a>,
    function_names: &'a [&'a str],
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    parent: Option<&'a LoweringContext<'a>>,
    class_super_binding: Option<ClassSuperBinding>,
    class_private_names: Option<std::rc::Rc<[String]>>,
) -> Result<(FunctionBodyOutput, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    // §14.1.1 Directive prologues — `"use strict"` is already the
    // default for ES modules, and classes / methods are strict
    // per spec regardless. Other string-literal directives are
    // silently ignored (the spec allows implementations to
    // reserve additional directive strings; nothing requires us
    // to honour them). Treat the whole prologue as metadata.
    let _ = &body.directives;

    let mut builder = BytecodeBuilder::new();
    let mut ctx = LoweringContext::with_parent(
        layout,
        function_names,
        module_functions,
        parent,
        class_super_binding,
        class_private_names,
    );

    // §14.1.21 FunctionDeclarationInstantiation — evaluate default
    // initializers for any param whose caller-supplied value is
    // `undefined`, then materialise the rest parameter's array
    // from `activation.overflow_args`. Both run before any body
    // statement so `Ldar r_param` later in the body sees a
    // definite value.
    emit_default_initializers(&mut builder, &mut ctx, layout)?;
    emit_param_destructuring(&mut builder, &mut ctx, layout)?;
    emit_rest_parameter(&mut builder, &mut ctx, layout)?;

    // Empty function body — synthesise `LdaUndefined; Return` so
    // the function exits per §15.2.1 FunctionBody evaluation
    // (falls through to `return undefined`). This lets
    // `function f() {}`, `() => {}`, and empty class-method
    // bodies all compile.
    let Some((last, rest)) = body.statements.split_last() else {
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined (empty body): {err:?}"))
        })?;
        builder.emit(Opcode::Return, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode Return (empty body): {err:?}"))
        })?;
        let exception_handlers = ctx.take_exception_handlers(&builder)?;
        let bytecode_len = builder.pc();
        let closure_table = ctx.take_closure_table(bytecode_len);
        let bytecode = builder
            .finish()
            .map_err(|err| SourceLoweringError::Internal(format!("finalise bytecode: {err:?}")))?;
        let captures = ctx.take_captures();
        return Ok((
            FunctionBodyOutput {
                bytecode,
                local_count: ctx.local_count(),
                temp_count: ctx.temp_count(),
                feedback_slot_count: ctx.feedback_slot_count(),
                feedback_slot_kinds: ctx.take_feedback_slot_kinds(),
                property_names: ctx.take_property_names(),
                float_constants: ctx.take_float_constants(),
                string_literals: ctx.take_string_literals(),
                bigint_constants: ctx.take_bigint_constants(),
                regexp_literals: ctx.take_regexp_literals(),
                exceptions: crate::exception::ExceptionTable::new(exception_handlers),
                closures: closure_table,
                source_map: ctx.take_source_map(),
            },
            captures,
        ));
    };

    // Two tail shapes are accepted:
    //   1. Explicit `return <expr>;` — lower the expression into
    //      acc, then `Return`. Matches the historical M6 contract.
    //   2. Any other statement — lower it as usual, then synthesize
    //      `LdaUndefined; Return` so the function exits with the
    //      undefined completion per §15.2.1 (FunctionBody evaluation
    //      falls through to `return undefined` when no explicit
    //      return is taken). This unlocks the natural
    //      `function main() { console.log("hi"); }` shape — prior
    //      to M19 the lowering required a spurious trailing
    //      `return` which is not how real JS is written.
    //
    // Bare `return;` with no argument is lowered by the second arm
    // because oxc represents it as a `ReturnStatement` with
    // `argument == None`, which `lower_nested_statement` handles as
    // `LdaUndefined; Return` directly.
    lower_function_top_statement_list(&mut builder, &mut ctx, rest)?;
    let needs_synthetic_return = match last {
        Statement::ReturnStatement(ret) if ret.argument.is_some() => {
            // D2: the trailing-return fast path bypasses
            // `lower_top_statement`, so record the source
            // location here to keep stack traces accurate for
            // the most common final statement.
            ctx.record_source_location(builder.pc(), last.span().start);
            let argument = ret.argument.as_ref().expect("checked Some above");
            lower_return_expression(&mut builder, &ctx, argument)?;
            builder
                .emit(Opcode::Return, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
            false
        }
        // Arrow concise body — oxc wraps `() => expr` as a
        // FunctionBody with a single `ExpressionStatement`
        // containing the expression. §15.3 specifies that this
        // form is semantically `() => { return expr; }`, so we
        // lower it as an implicit return. Detected by checking
        // that this is the ONLY statement in the body (no
        // preceding `rest`) and its expression can be any
        // acc-producing shape, not just call / assign / update.
        Statement::ExpressionStatement(expr_stmt)
            if rest.is_empty() && body.statements.len() == 1 =>
        {
            // Only take this path for expressions the top-statement
            // lowerer wouldn't already have accepted (call, assign,
            // update). For those we fall through to the default
            // catchall below, keeping the pre-existing semantics
            // (call expression statement leaves `undefined` as the
            // implicit return, matching regular function bodies).
            if matches!(
                expr_stmt.expression,
                Expression::CallExpression(_)
                    | Expression::AssignmentExpression(_)
                    | Expression::UpdateExpression(_)
            ) {
                lower_top_statement(&mut builder, &mut ctx, last)?;
                true
            } else {
                lower_return_expression(&mut builder, &ctx, &expr_stmt.expression)?;
                builder.emit(Opcode::Return, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Return (arrow concise body): {err:?}"
                    ))
                })?;
                false
            }
        }
        _ => {
            // Lower the statement (call-statement, assignment, if,
            // while, block, bare `return;`, …) — it must be a
            // shape `lower_top_statement` already accepts.
            lower_function_top_statement_list(&mut builder, &mut ctx, std::slice::from_ref(last))?;
            true
        }
    };
    if needs_synthetic_return {
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined (synth return): {err:?}"))
        })?;
        builder.emit(Opcode::Return, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode Return (synth): {err:?}"))
        })?;
    }

    // Resolve pending exception handlers to concrete PCs before
    // `finish()` drops the builder's label state.
    let exception_handlers = ctx.take_exception_handlers(&builder)?;
    let bytecode_len = builder.pc();
    let closure_table = ctx.take_closure_table(bytecode_len);

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("finalise bytecode: {err:?}")))?;

    let captures = ctx.take_captures();
    Ok((
        FunctionBodyOutput {
            bytecode,
            local_count: ctx.local_count(),
            temp_count: ctx.temp_count(),
            feedback_slot_count: ctx.feedback_slot_count(),
            feedback_slot_kinds: ctx.take_feedback_slot_kinds(),
            property_names: ctx.take_property_names(),
            float_constants: ctx.take_float_constants(),
            string_literals: ctx.take_string_literals(),
            bigint_constants: ctx.take_bigint_constants(),
            regexp_literals: ctx.take_regexp_literals(),
            exceptions: crate::exception::ExceptionTable::new(exception_handlers),
            closures: closure_table,
            source_map: ctx.take_source_map(),
        },
        captures,
    ))
}
///
/// Capture analysis: the inner function's body is lowered through
/// a recursive `lower_inner_function` call that passes the outer
/// context as the "lookup parent". Any identifier inside the inner
/// function that can't be resolved to a local/param/global is
/// looked up in the outer's bindings:
/// - Outer local / param → `CaptureDescriptor::Register(reg)` —
///   the outer frame promotes that slot into an open upvalue
///   cell at `CreateClosure` time (via
///   `capture_bytecode_register_upvalue`), and the inner closure
///   uses `LdaUpvalue <idx>` to read / `StaUpvalue <idx>` to write.
/// - Outer-outer capture → a nested closure references an
///   already-captured binding; emitted as
///   `CaptureDescriptor::Upvalue(UpvalueId)` so the dispatcher
///   re-captures the parent closure's upvalue cell.
///
/// Bytecode shape:
///
/// ```text
///   CreateClosure <inner_idx>, 0
/// ```
///
/// The `ClosureTable` entry at this PC carries the callee's
/// `FunctionIndex` plus the list of `CaptureDescriptor`s in
/// upvalue-index order.
pub(super) fn lower_function_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    func: &'a Function<'a>,
) -> Result<(), SourceLoweringError> {
    // Lower the inner function first — the recursive lowering
    // collects the list of outer bindings it captured. The
    // captures come back as a `Vec<CaptureDescriptor>`; each
    // element's slot index matches the inner function's
    // `LdaUpvalue <idx>` operands.
    let (inner_idx, captures) = lower_inner_function_with_captures(func, ctx)?;
    {
        let mut fns = ctx.module_functions.borrow_mut();
        let target = &mut fns[inner_idx as usize];
        if func.r#async {
            target.set_async(true);
        }
        if func.generator {
            target.set_generator(true);
        }
    }

    let pc = builder.pc();
    let flags = match (func.r#async, func.generator) {
        (true, true) => crate::object::ClosureFlags::async_generator(),
        (true, false) => crate::object::ClosureFlags::async_fn(),
        (false, true) => crate::object::ClosureFlags::generator(),
        (false, false) => crate::object::ClosureFlags::normal(),
    };
    let template = crate::closure::ClosureTemplate::with_flags(
        crate::module::FunctionIndex(inner_idx),
        captures,
        flags,
    );
    ctx.record_closure_template(pc, template);

    // Emit `CreateClosure <idx>, 0`. The second operand carries
    // closure flags — dispatch reads them from the closure template
    // at the PC, so the imm is conventional (zero).
    builder
        .emit(
            Opcode::CreateClosure,
            &[Operand::Idx(inner_idx), Operand::Imm(0)],
        )
        .map_err(|err| SourceLoweringError::Internal(format!("encode CreateClosure: {err:?}")))?;
    Ok(())
}

/// Lowers `(args) => expr` / `(args) => { body }` — an arrow
/// function — into a closure value. Same shape as
/// `FunctionExpression` with two differences:
/// - Arrows cannot be generators; `async` rejected until M33.
/// - Arrows have lexical `this`. M26 doesn't introduce any `this`
///   support in the source compiler (classes and `this` land in
///   M27+), so lexical-`this` is automatically satisfied: every
///   arrow just lowers as a regular closure body and neither the
///   arrow nor its container uses `this`.
///
/// §15.3 Arrow Function Definitions.
pub(super) fn lower_arrow_function_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    arrow: &'a ArrowFunctionExpression<'a>,
) -> Result<(), SourceLoweringError> {
    // oxc synthesises the arrow body as a `FunctionBody` whose
    // single statement is a `ReturnStatement` for concise
    // `() => expr` form. Block-body arrows already have a
    // regular FunctionBody. Either case flows through
    // `lower_inner_callable` unchanged — no special-casing of
    // `arrow.expression` needed.
    let (inner_idx, captures) = lower_inner_callable(ctx, &arrow.params, &arrow.body, None)?;
    if arrow.r#async {
        let mut fns = ctx.module_functions.borrow_mut();
        fns[inner_idx as usize].set_async(true);
    }
    let pc = builder.pc();
    let template = crate::closure::ClosureTemplate::with_flags(
        crate::module::FunctionIndex(inner_idx),
        captures,
        if arrow.r#async {
            crate::object::ClosureFlags::async_arrow()
        } else {
            crate::object::ClosureFlags::arrow()
        },
    );
    ctx.record_closure_template(pc, template);
    builder
        .emit(
            Opcode::CreateClosure,
            &[Operand::Idx(inner_idx), Operand::Imm(0)],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode CreateClosure (arrow): {err:?}"))
        })?;
    Ok(())
}

/// Lowers `function foo() { … }` inside another function body.
/// Treated as hoisting-free shorthand for `let foo = function() {
/// … };` — the name is bound as a `const` local so accidental
/// reassignment rejects, and the closure's captures follow the
/// same parent-chain resolution the FunctionExpression path uses.
///
/// M25 simplification: spec-accurate hoisting (§14.1.11) isn't
/// implemented — forward references to a nested
/// FunctionDeclaration before its lexical position would surface
/// as `unbound_identifier`. Real code typically declares before
/// use, so this is a narrow corner.
pub(super) fn lower_nested_function_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    func: &'a Function<'a>,
) -> Result<(), SourceLoweringError> {
    let name_ident = func
        .id
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_function", func.span))?;
    let name = name_ident.name.as_str();

    // Lower the inner function + record captures against the
    // enclosing context, same as FunctionExpression.
    let (inner_idx, captures) = lower_inner_function_with_captures(func, ctx)?;
    {
        let mut fns = ctx.module_functions.borrow_mut();
        let target = &mut fns[inner_idx as usize];
        if func.r#async {
            target.set_async(true);
        }
        if func.generator {
            target.set_generator(true);
        }
    }
    let pc = builder.pc();
    let flags = match (func.r#async, func.generator) {
        (true, true) => crate::object::ClosureFlags::async_generator(),
        (true, false) => crate::object::ClosureFlags::async_fn(),
        (false, true) => crate::object::ClosureFlags::generator(),
        (false, false) => crate::object::ClosureFlags::normal(),
    };
    let template = crate::closure::ClosureTemplate::with_flags(
        crate::module::FunctionIndex(inner_idx),
        captures,
        flags,
    );
    ctx.record_closure_template(pc, template);
    builder
        .emit(
            Opcode::CreateClosure,
            &[Operand::Idx(inner_idx), Operand::Imm(0)],
        )
        .map_err(|err| SourceLoweringError::Internal(format!("encode CreateClosure: {err:?}")))?;

    // Bind the produced closure to a local with the function's
    // name (`const`-like — reassigning would rebind the name to
    // a different value which the spec disallows for a
    // declaration binding).
    let slot = ctx.allocate_local(name, true, name_ident.span)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (nested function binding): {err:?}"))
        })?;
    ctx.mark_initialized(name)?;
    Ok(())
}


/// Lowers `new Foo(args)` — allocates the receiver from
/// `Foo.prototype`, invokes the constructor with
/// `this = receiver` + `new.target = Foo`, and applies the
/// §9.2.2.1 return override (keep explicit object return, fall
/// back to the allocated receiver otherwise).
///
/// Bytecode shape:
///
/// ```text
///   <lower callee>; Star r_callee
///   <lower arg_0>;  Star r_arg0
///   …
///   Construct r_callee, r_callee, RegList{base=r_arg0, count=argc}
/// ```
///
/// `new.target` uses the same register as the target — callers
/// that need a distinct `new.target` would have to be written
/// through class inheritance, which lands with `extends` (M28).
pub(super) fn lower_new_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a NewExpression<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;
    let has_spread = expr
        .arguments
        .iter()
        .any(|arg| matches!(arg, Argument::SpreadElement(_)));
    if has_spread {
        return lower_new_expression_with_spread(builder, ctx, expr);
    }
    let argc = RegisterIndex::try_from(expr.arguments.len())
        .map_err(|_| SourceLoweringError::Internal("new argument count exceeds u16".into()))?;
    let callee_temp = ctx.acquire_temps(1)?;
    let args_base = if argc == 0 {
        0
    } else {
        ctx.acquire_temps(argc)
            .inspect_err(|_| ctx.release_temps(1))?
    };
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &expr.callee)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (new callee): {err:?}"))
            })?;
        for (offset, arg) in expr.arguments.iter().enumerate() {
            let arg_expr = match arg {
                Argument::SpreadElement(_) => unreachable!("rejected above"),
                other => other.to_expression(),
            };
            lower_return_expression(builder, ctx, arg_expr)?;
            let slot = args_base
                .checked_add(RegisterIndex::try_from(offset).map_err(|_| {
                    SourceLoweringError::Internal("new argument offset overflow".into())
                })?)
                .ok_or_else(|| SourceLoweringError::Internal("new arg slot overflow".into()))?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (new arg): {err:?}"))
                })?;
        }
        let call_pc = builder
            .emit(
                Opcode::Construct,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::RegList {
                        base: u32::from(args_base),
                        count: u32::from(argc),
                    },
                ],
            )
            .map_err(|err| SourceLoweringError::Internal(format!("encode Construct: {err:?}")))?;
        ctx.attach_call_feedback(builder, call_pc);
        Ok(())
    })();
    if argc > 0 {
        ctx.release_temps(argc);
    }
    ctx.release_temps(1);
    lower
}

/// Spread-argument `new C(...args)`. Builds a single Array from
/// the spread + plain arguments and dispatches via
/// `ConstructSpread` — the same shape the existing
/// `Construct` path uses, just with the spread arg-window.
fn lower_new_expression_with_spread<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a NewExpression<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;
    let callee_temp = ctx.acquire_temps(1)?;
    let args_base = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &expr.callee)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (new spread callee): {err:?}"))
            })?;
        builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode CreateArray (new spread): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(args_base))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (new spread args): {err:?}"))
            })?;
        for arg in expr.arguments.iter() {
            match arg {
                Argument::SpreadElement(spread) => {
                    lower_return_expression(builder, ctx, &spread.argument)?;
                    builder
                        .emit(
                            Opcode::SpreadIntoArray,
                            &[Operand::Reg(u32::from(args_base))],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode SpreadIntoArray (new): {err:?}"
                            ))
                        })?;
                }
                other => {
                    lower_return_expression(builder, ctx, other.to_expression())?;
                    builder
                        .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(args_base))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode ArrayPush (new spread arg): {err:?}"
                            ))
                        })?;
                }
            }
        }
        let call_pc = builder
            .emit(
                Opcode::ConstructSpread,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::RegList {
                        base: u32::from(args_base),
                        count: 1,
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode ConstructSpread: {err:?}"))
            })?;
        ctx.attach_call_feedback(builder, call_pc);
        Ok(())
    })();
    ctx.release_temps(2);
    lower
}

/// Recursively lowers a nested `Function` (the body of a
/// `FunctionExpression` or a nested `FunctionDeclaration`) and
/// appends its `VmFunction` to the shared module function list.
/// Returns the assigned `FunctionIndex` as a raw `u32`.
///
/// M25 Phase A: inner functions see an empty outer scope — no
/// captures allowed. Any reference to a name that isn't a
/// local / param / whitelisted global surfaces as
/// `unbound_identifier` from the regular identifier-resolution
/// path. Phase B rewires that branch to synthesise captures.
/// Lowers a nested function and returns `(function_index,
/// captures)`. Captures list drives the parent's
/// `ClosureTemplate` — each entry matches a `LdaUpvalue idx` /
/// `StaUpvalue idx` inside the inner body.
fn lower_inner_function_with_captures<'a>(
    func: &'a Function<'a>,
    outer: &LoweringContext<'a>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    let body = func
        .body
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("declared_only_function", func.span))?;
    let name = func.id.as_ref().map(|ident| ident.name.as_str().to_owned());
    lower_inner_callable(outer, &func.params, body, name)
}

/// Shared core for lowering a nested callable (FunctionExpression,
/// ArrowFunctionExpression, or nested FunctionDeclaration). Takes
/// params + body explicitly so the per-AST-shape wrappers can
/// funnel through a single path.
///
/// Allocates a fresh module function index, lowers the body with
/// the outer context as capture parent, produces a `VmFunction`,
/// pushes it to the shared module list, and returns
/// `(idx, captures)` so the caller can record a
/// `ClosureTemplate`.
fn lower_inner_callable<'a>(
    outer: &LoweringContext<'a>,
    params: &'a FormalParameters<'a>,
    body: &'a FunctionBody<'a>,
    name: Option<String>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    lower_inner_callable_with_super(outer, params, body, name, None, None)
}

/// M28/M29 variant of [`lower_inner_callable`] that threads
/// class-scope metadata into the inner function's `LoweringContext`
/// so class methods and constructors can (1) validate `super.x` /
/// `super(args)` uses and (2) resolve `this.#x` / `obj.#x` against
/// the surrounding class's private-name list. Callers outside
/// `lower_class_body_core` always pass `None` for both.
pub(super) fn lower_inner_callable_with_super<'a>(
    outer: &LoweringContext<'a>,
    params: &'a FormalParameters<'a>,
    body: &'a FunctionBody<'a>,
    name: Option<String>,
    class_super_binding: Option<ClassSuperBinding>,
    class_private_names: Option<std::rc::Rc<[String]>>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    let params_layout = analyze_params(params)?;
    let param_count = params_layout.param_slot_count();

    let (body_out, captures) = lower_function_body_with_parent(
        body,
        params,
        &params_layout,
        outer.function_names,
        std::rc::Rc::clone(&outer.module_functions),
        Some(outer),
        class_super_binding,
        class_private_names,
    )?;

    let layout = FrameLayout::new(1, param_count, body_out.local_count, body_out.temp_count)
        .map_err(|err| SourceLoweringError::Internal(format!("frame layout invalid: {err:?}")))?;
    let feedback_layout = feedback_layout_from_kinds(&body_out.feedback_slot_kinds);
    let side_tables = crate::module::FunctionSideTables::new(
        body_out.property_names,
        body_out.string_literals,
        body_out.float_constants,
        body_out.bigint_constants,
        body_out.closures,
        Default::default(),
        body_out.regexp_literals,
    );
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        body_out.exceptions,
        body_out.source_map,
    );
    let inner = VmFunction::new(name, layout, body_out.bytecode, tables);

    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("module function index overflow".into()))?;
    fns.push(inner);
    Ok((idx, captures))
}
