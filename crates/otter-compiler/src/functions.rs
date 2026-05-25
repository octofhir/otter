//! Function, arrow, and callable object lowering.
//!
//! # Contents
//! - full function compilation
//! - arrow function compilation
//! - callable emission
//!
//! # Invariants
//! - Nested functions are registered in the shared module builder.
//!
//! # See also
//! - `params` and `function_context`

use crate::*;

pub(crate) fn compile_function_full(
    parent: &mut Compiler,
    name: &str,
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &Option<oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody<'_>>>,
    span: (u32, u32),
    is_async: bool,
    is_generator: bool,
    force_strict: bool,
) -> Result<(u32, Vec<u32>), CompileError> {
    let is_async_generator = is_async && is_generator;
    let module = Rc::clone(&parent.top_mut().module);
    let body_has_strict_directive = match body {
        Some(b) => b.has_use_strict_directive(),
        None => false,
    };
    let function_is_strict = force_strict || parent.is_strict || body_has_strict_directive;
    let simple_params = formal_parameters_are_simple(params);
    let allow_duplicate_formals = !function_is_strict && simple_params;
    let needs_arguments = body_references_arguments(params, body.as_deref());
    let uses_mapped_arguments = needs_arguments && !function_is_strict && simple_params;
    validate_formal_parameter_names(params, function_is_strict, allow_duplicate_formals, span)?;
    let mut child = FunctionContext::new(Rc::clone(&module)).with_strict(function_is_strict);
    if let Some(b) = body {
        child.captured_names = capture::analyze_function(Some(params), b);
    }
    if uses_mapped_arguments {
        child.mapped_argument_names = simple_formal_names(params).into_iter().collect();
    }
    parent.push(child);
    parent.enter_scope();

    // Reserve raw argv slots up front so destructuring / defaults
    // can address them by ordinal. The compiler's scratch counter
    // tracks them so subsequent register allocations don't collide.
    let param_count = u16::try_from(params.items.len()).expect("too many parameters");
    parent.scratch = param_count;
    let has_rest = params.rest.is_some();

    // Reserve the function's id ahead of compilation so the body
    // can reference its own name (recursion).
    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        is_strict: function_is_strict,
        ..Default::default()
    });

    // Bind every formal parameter, in source order. Side-effects
    // (default-value evaluation, iterator-protocol calls for array
    // patterns) follow the spec's per-call ordering.
    for (ordinal, param) in params.items.iter().enumerate() {
        compile_formal_parameter(
            parent,
            ordinal as u16,
            &param.pattern,
            param.initializer.as_deref(),
            span,
            allow_duplicate_formals,
        )?;
    }
    if let Some(rest) = &params.rest {
        compile_rest_parameter(parent, &rest.rest.argument, span)?;
    }
    let mapped_argument_bindings = if uses_mapped_arguments {
        mapped_formal_parameter_bindings(parent, params)
    } else {
        Vec::new()
    };

    // Bind self-name for recursion. Emit a MakeFunction (no
    // captures yet — the function value referencing itself doesn't
    // need its own captures bound here).
    let self_storage = parent.declare_binding(name, false, span)?;
    let const_idx = parent.intern_function_id(function_id);
    let tmp = parent.alloc_scratch();
    parent.emit(
        Op::MakeFunction,
        [Operand::Register(tmp), Operand::ConstIndex(const_idx)],
        span,
    );
    parent.emit_store_storage(tmp, self_storage, span);
    parent.mark_initialized(name);

    // §10.2.11 FunctionDeclarationInstantiation step 28 — hoist
    // every `var`-declared name in the body to the function scope
    // and pre-bind it to `undefined`. Reads before the source-level
    // declaration site observe the hoisted `undefined` (no TDZ).
    if needs_arguments && parent.lookup_binding("arguments").is_none() {
        // §10.2.11 FunctionDeclarationInstantiation step 22 — bind
        // `arguments` in the function scope before any var/lex
        // declaration so user code reading it gets the array.
        // Skip if a parameter named `arguments` already exists.
        let storage = parent.declare_binding("arguments", false, span)?;
        let tmp = parent.alloc_scratch();
        parent.emit(Op::CollectArguments, [Operand::Register(tmp)], span);
        parent.emit_store_storage(tmp, storage, span);
        parent.mark_initialized("arguments");
    }
    if let Some(body) = body {
        let mut var_names: Vec<String> = Vec::new();
        hoist_var_names(&body.statements, &mut var_names);
        pre_declare_var_bindings(parent, &var_names, span)?;
        // Pre-declare lexical bindings (TDZ) so hoisted nested
        // functions can capture forward references.
        let mut lex_names: Vec<(String, bool)> = Vec::new();
        hoist_lexical_names(&body.statements, &mut lex_names);
        pre_declare_lexical_bindings(parent, &lex_names, span)?;
        // §10.2.11 step 30 — function declarations hoist to the
        // function scope. Pre-emitting their closure stores here
        // means calls placed textually above the declaration
        // resolve correctly.
        hoist_function_declarations(parent, &body.statements)?;
        if is_generator {
            parent.emit(Op::GeneratorStart, vec![], span);
        }
        for stmt in &body.statements {
            compile_statement(parent, stmt)?;
        }
    }
    parent.exit_scope();
    // Implicit `return undefined;` at the function tail.
    parent.emit(Op::ReturnUndefined, vec![], span);

    let child = parent.pop();
    let captures = child.parent_captures.clone();
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = child.scratch;
    slot.param_count = param_count;
    slot.has_rest = has_rest;
    slot.is_async = is_async;
    slot.is_generator = is_generator;
    slot.is_async_generator = is_async_generator;
    slot.needs_arguments = needs_arguments;
    slot.arguments_object_kind = if uses_mapped_arguments {
        ArgumentsObjectKind::Mapped
    } else {
        ArgumentsObjectKind::Unmapped
    };
    slot.mapped_argument_bindings = mapped_argument_bindings;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}

/// Compile an arrow function. Two body shapes share the same
/// lowering:
///
/// - `() => expr` (expression body): one synthetic
///   `ReturnValue(expr)`.
/// - `() => { ... }` (block body): existing function-body
///   compilation, with an implicit `ReturnUndefined` tail.
///
/// Captures from the enclosing scope flow through the same
/// upvalue mechanism as nested function declarations — see
/// [`capture`]. The arrow has no `this` of its own (foundation
/// slice doesn't model `this` yet — task 23).
pub(crate) fn compile_arrow_function(
    parent: &mut Compiler,
    arrow: &oxc_ast::ast::ArrowFunctionExpression<'_>,
    span: (u32, u32),
) -> Result<(u32, Vec<u32>), CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let function_is_strict = parent.is_strict || arrow.body.has_use_strict_directive();
    validate_formal_parameter_names(&arrow.params, function_is_strict, false, span)?;
    let mut child = FunctionContext::new(Rc::clone(&module)).with_strict(function_is_strict);
    child.captured_names = capture::analyze_arrow(arrow);
    parent.push(child);
    parent.enter_scope();

    let param_count = u16::try_from(arrow.params.items.len()).expect("too many parameters");
    parent.scratch = param_count;
    let has_rest = arrow.params.rest.is_some();

    // Reserve the function record up front so we can emit
    // `MakeFunction` / `MakeClosure` for the result later.
    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: "<arrow>".to_string(),
        span,
        is_strict: function_is_strict,
        ..Default::default()
    });

    for (ordinal, param) in arrow.params.items.iter().enumerate() {
        compile_formal_parameter(
            parent,
            ordinal as u16,
            &param.pattern,
            param.initializer.as_deref(),
            span,
            false,
        )?;
    }
    if let Some(rest) = &arrow.params.rest {
        compile_rest_parameter(parent, &rest.rest.argument, span)?;
    }

    if arrow.expression {
        // `() => expr` — body is a single ExpressionStatement
        // whose expression is the implicit return value.
        let stmt = arrow
            .body
            .statements
            .first()
            .ok_or(CompileError::Unsupported {
                node: "ArrowFunction: empty expression body".to_string(),
                span,
            })?;
        let Statement::ExpressionStatement(es) = stmt else {
            return Err(CompileError::Unsupported {
                node: "ArrowFunction: malformed expression body".to_string(),
                span,
            });
        };
        let inner_span = (es.span.start, es.span.end);
        let reg = compile_expr(parent, &es.expression, inner_span)?;
        parent.emit(Op::ReturnValue, [Operand::Register(reg)], inner_span);
    } else {
        // §10.2.11 FunctionDeclarationInstantiation — block-body
        // arrow functions own a regular function scope and must
        // pre-hoist `var` / lexical / function-declaration bindings
        // before walking the body, exactly like the regular
        // `compile_function_declaration_body` pass above. Without
        // this, nested `var x = …` inside the arrow body fails the
        // `var \`x\` not pre-hoisted` invariant check at the
        // `Statement::VariableDeclaration` arm.
        let mut var_names: Vec<String> = Vec::new();
        hoist_var_names(&arrow.body.statements, &mut var_names);
        pre_declare_var_bindings(parent, &var_names, span)?;
        let mut lex_names: Vec<(String, bool)> = Vec::new();
        hoist_lexical_names(&arrow.body.statements, &mut lex_names);
        pre_declare_lexical_bindings(parent, &lex_names, span)?;
        hoist_function_declarations(parent, &arrow.body.statements)?;
        for stmt in &arrow.body.statements {
            compile_statement(parent, stmt)?;
        }
        parent.emit(Op::ReturnUndefined, vec![], span);
    }
    parent.exit_scope();

    let child = parent.pop();
    let captures = child.parent_captures.clone();
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = child.scratch;
    slot.param_count = param_count;
    slot.has_rest = has_rest;
    slot.is_async = arrow.r#async;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.is_arrow = true;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}

/// Emit the right "make a callable into `dst`" instruction:
/// [`Op::MakeFunction`] when the inner function captures nothing,
/// [`Op::MakeClosure`] otherwise.
///
/// Arrow functions always go through [`Op::MakeClosure`] (even with
/// zero non-`this` captures) so the runtime can snapshot the
/// enclosing frame's `this` into the closure value at construction
/// time. Regular function declarations / expressions take `this`
/// from the call site and use the lighter `MakeFunction` form when
/// they have no captures.
pub(crate) fn emit_make_callable(
    cx: &mut Compiler,
    dst: u16,
    function_const: u32,
    captures: &[u32],
    is_arrow: bool,
    span: (u32, u32),
) -> Result<(), CompileError> {
    if captures.is_empty() && !is_arrow {
        cx.emit(
            Op::MakeFunction,
            [Operand::Register(dst), Operand::ConstIndex(function_const)],
            span,
        );
        return Ok(());
    }
    // `MakeClosure` operand layout is `[dst, fn_const, count,
    // capture0, …, captureN-1]`; the wire encoder caps the total
    // operand count at `u8::MAX` (255), so the capture-count payload
    // tops out at 252. Beyond that we surface a `CompileError`
    // instead of panicking inside the bytecode writer.
    const MAX_CAPTURES: usize = u8::MAX as usize - 3;
    if captures.len() > MAX_CAPTURES {
        return Err(CompileError::Unsupported {
            node: format!(
                "closure capturing {} upvalues exceeds the {} limit of `Op::MakeClosure`",
                captures.len(),
                MAX_CAPTURES
            ),
            span,
        });
    }
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + captures.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::ConstIndex(function_const));
    operands.push(Operand::ConstIndex(captures.len() as u32));
    for &parent_idx in captures {
        operands.push(Operand::Imm32(parent_idx as i32));
    }
    cx.emit(Op::MakeClosure, operands, span);
    Ok(())
}
