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
    let is_method = std::mem::take(&mut parent.next_fn_is_method);
    let static_home = std::mem::take(&mut parent.next_fn_static_home);
    let no_self_name = std::mem::take(&mut parent.next_fn_no_self_name);
    let module = Rc::clone(&parent.top_mut().module);
    let body_has_strict_directive = match body {
        Some(b) => b.has_use_strict_directive(),
        None => false,
    };
    let function_is_strict = force_strict || parent.is_strict || body_has_strict_directive;
    let simple_params = formal_parameters_are_simple(params);
    // §15.4.1 — MethodDefinition uses UniqueFormalParameters even in
    // sloppy code.
    let allow_duplicate_formals = !function_is_strict && simple_params && !is_method;
    // A direct eval body may reference `arguments` dynamically, so
    // its presence forces the arguments object to materialize even
    // when the enclosing body never names it (§19.2.1.3).
    let contains_direct_eval = body
        .as_ref()
        .is_some_and(|b| capture::body_contains_direct_eval(Some(params), b));
    let needs_arguments =
        body_references_arguments(params, body.as_deref()) || contains_direct_eval;
    let uses_mapped_arguments = needs_arguments && !function_is_strict && simple_params;
    validate_formal_parameter_names(params, function_is_strict, allow_duplicate_formals, span)?;
    let active_with_envs = parent.active_with_envs.clone();
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(function_is_strict)
        .with_module_url(parent.module_url.clone());
    child.super_home_static = static_home;
    child.active_with_envs = active_with_envs;
    child.is_async_generator = is_async_generator;
    // §10.2.11 — every non-arrow function's variable environment
    // binds `arguments` (as the arguments object, a parameter, or a
    // body declaration), which arms the §19.2.1.3 direct-eval check
    // during parameter initialization.
    child.binds_arguments = true;
    if let Some(b) = body {
        child.captured_names = capture::analyze_function(Some(params), b);
        // §19.2.1.3 — a direct eval body reads and writes caller
        // bindings through upvalue cells, so promote every
        // function-scope binding (not just statically captured ones).
        if contains_direct_eval {
            child
                .captured_names
                .extend(capture::all_own_names(Some(params), b));
        }
    }
    child.contains_direct_eval = contains_direct_eval;
    if uses_mapped_arguments {
        child.mapped_argument_names = simple_formal_names(params).into_iter().collect();
    }
    child.reserve_known_own_upvalues();
    parent.push(child);
    parent.enter_scope();
    // A non-arrow function owns its `new.target` — the
    // field-initializer signal does not propagate into its body.
    let saved_field_init = parent.in_field_initializer;
    parent.in_field_initializer = false;

    // Reserve raw argv slots up front so destructuring / defaults
    // can address them by ordinal. The compiler's scratch counter
    // tracks them so subsequent register allocations don't collide.
    let param_count = u16::try_from(params.items.len()).expect("too many parameters");
    let length = formal_parameter_length(params);
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
        module_url: parent.module_url.clone(),
        ..Default::default()
    });

    predeclare_formal_parameters(parent, params, allow_duplicate_formals, span)?;
    // §10.2.11 step 22 — bind `arguments` BEFORE
    // IteratorBindingInitialization (step 24), so a parameter
    // default expression like `x = arguments[0]` resolves the
    // arguments object. Skip if a formal named `arguments` exists.
    if needs_arguments && parent.lookup_binding("arguments").is_none() {
        let storage = parent.declare_binding("arguments", false, span)?;
        let tmp = parent.alloc_scratch();
        parent.emit(Op::CollectArguments, [Operand::Register(tmp)], span);
        parent.emit_store_storage(tmp, storage, span);
        parent.mark_initialized("arguments");
    }
    // Bind every formal parameter, in source order. Side-effects
    // (default-value evaluation, iterator-protocol calls for array
    // patterns) follow the spec's per-call ordering.
    parent.in_param_init = true;
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
    parent.in_param_init = false;
    let mapped_argument_bindings = if uses_mapped_arguments {
        mapped_formal_parameter_bindings(parent, params)
    } else {
        Vec::new()
    };

    // Bind self-name for recursion. Capture operands are finalized after
    // body lowering because parent captures are discovered lazily.
    // MethodDefinition bodies get NO self-name binding: a method's
    // property name is not a binding inside it (§15.4), and a class
    // constructor's name must resolve to the §15.7.14 class-scope
    // binding, not to a re-made closure of itself.
    let fn_self_immutable = std::mem::take(&mut parent.fn_self_immutable_hint);
    let self_make_idx = if is_method || no_self_name {
        None
    } else {
        let self_storage = parent.declare_binding(name, false, span)?;
        if fn_self_immutable {
            parent.top_mut().mark_fn_self_name(name);
        }
        let const_idx = parent.intern_function_id(function_id);
        let tmp = parent.alloc_scratch();
        let idx = parent.code.len();
        parent.emit(
            Op::MakeFunction,
            [Operand::Register(tmp), Operand::ConstIndex(const_idx)],
            span,
        );
        parent.emit_store_storage(tmp, self_storage, span);
        parent.mark_initialized(name);
        Some((idx, tmp, const_idx))
    };

    // §10.2.11 FunctionDeclarationInstantiation step 28 — hoist
    // every `var`-declared name in the body to the function scope
    // and pre-bind it to `undefined`. Reads before the source-level
    // declaration site observe the hoisted `undefined` (no TDZ).
    let mut direct_eval_meta: Vec<otter_bytecode::DirectEvalBinding> = Vec::new();
    if let Some(body) = body {
        let mut var_names: Vec<String> = Vec::new();
        hoist_var_names(&body.statements, &mut var_names);
        // §B.3.3.1 — parameter names (and the arguments object's
        // implicit binding) block the sloppy block-level function
        // var-scope extension; everything bound so far is a parameter
        // or the function self-name.
        let mut annex_blocked: std::collections::HashSet<String> = parent
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.keys().cloned())
            .collect();
        annex_blocked.insert("arguments".to_string());
        pre_declare_annex_b_functions(parent, &body.statements, &annex_blocked, span)?;
        pre_declare_var_bindings(parent, &var_names, span)?;
        // Pre-declare lexical bindings (TDZ) so hoisted nested
        // functions can capture forward references.
        let mut lex_names: Vec<(String, bool)> = Vec::new();
        hoist_lexical_names(&body.statements, &mut lex_names);
        validate_no_param_lexical_conflict(params, &lex_names, span)?;
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
        if contains_direct_eval {
            capture_private_environment_for_eval(parent);
            capture_super_bindings_for_eval(parent);
            direct_eval_meta = collect_direct_eval_bindings(parent, &lex_names);
        }
    }
    parent.in_field_initializer = saved_field_init;
    parent.exit_scope();
    // Implicit `return undefined;` at the function tail.
    parent.emit(Op::ReturnUndefined, vec![], span);

    let mut child = parent.pop();
    if child.register_overflow {
        return Err(CompileError::Unsupported {
            node: "function body exhausts the 65535-register window".to_string(),
            span,
        });
    }

    let captures = child.parent_captures.clone();
    if !captures.is_empty()
        && let Some((self_make_idx, tmp, const_idx)) = self_make_idx
    {
        let self_captures: Vec<u32> = (0..captures.len())
            .map(|idx| child.own_upvalue_count as u32 + idx as u32)
            .collect();
        let instruction = child
            .code
            .get_mut(self_make_idx)
            .expect("self binding instruction is emitted before body");
        instruction.op = Op::MakeClosure;
        instruction.operands = make_closure_operands(tmp, const_idx, &self_captures, span)?.into();
    }
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = child.scratch_window();
    slot.param_count = param_count;
    slot.length = length;
    slot.has_rest = has_rest;
    slot.is_async = is_async;
    slot.is_generator = is_generator;
    slot.is_async_generator = is_async_generator;
    slot.is_method = is_method;
    slot.needs_arguments = needs_arguments;
    slot.arguments_object_kind = if uses_mapped_arguments {
        ArgumentsObjectKind::Mapped
    } else {
        ArgumentsObjectKind::Unmapped
    };
    slot.mapped_argument_bindings = mapped_argument_bindings;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.direct_eval_bindings = direct_eval_meta;
    slot.contains_direct_eval = contains_direct_eval;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}

/// Snapshot the function-scope bindings that live in upvalue cells as
/// a [`DirectEvalBinding`] table for `Op::Eval`. Called after the body
/// is compiled (every hoisted declaration has settled) and before the
/// function scope is exited.
pub(crate) fn collect_direct_eval_bindings(
    cx: &Compiler,
    lexical_names: &[(String, bool)],
) -> Vec<otter_bytecode::DirectEvalBinding> {
    let lexical: std::collections::HashSet<&str> = lexical_names
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    let Some(scope) = cx.scopes.first() else {
        return Vec::new();
    };
    let mut entries: Vec<otter_bytecode::DirectEvalBinding> = scope
        .bindings
        .iter()
        .filter_map(|(name, info)| match info.storage {
            BindingStorage::Upvalue { idx } => Some(otter_bytecode::DirectEvalBinding {
                name: name.clone(),
                upvalue: idx,
                lexical: lexical.contains(name.as_str()),
            }),
            BindingStorage::Register { .. } => None,
        })
        .collect();
    // Captured private-name / brand cells (class scope) ride along
    // so a direct eval can resolve `obj.#name` (§19.2.1.1
    // PrivateEnvironment inheritance).
    for (name, idx) in cx.captured_uv.iter() {
        if name.starts_with("__privsym_")
            || name.starts_with("__privbrand_")
            || name == crate::class::SUPER_HOME_NAME
            || name == crate::class::SUPER_STATIC_HOME_NAME
            || name == crate::class::SUPER_CTOR_NAME
        {
            entries.push(otter_bytecode::DirectEvalBinding {
                name: name.clone(),
                upvalue: *idx,
                lexical: true,
            });
        }
    }
    // `bindings` is hash-ordered; sort for deterministic bytecode.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// `true` when an `arguments` binding already exists in an arrow's
/// variable environment *during parameter instantiation* — i.e. a
/// formal parameter (or rest pattern) declares the name. Body
/// declarations don't count: their bindings are created only after
/// the parameters finish, so a direct eval in a parameter default
/// may legally var-declare `arguments` (§19.2.1.3). Arrows never
/// synthesize an arguments object of their own (§10.2.11).
fn arrow_binds_arguments(arrow: &oxc_ast::ast::ArrowFunctionExpression<'_>) -> bool {
    let mut names: Vec<String> = Vec::new();
    for param in &arrow.params.items {
        collect_pattern_var_names(&param.pattern, &mut names);
    }
    if let Some(rest) = &arrow.params.rest {
        collect_pattern_var_names(&rest.rest.argument, &mut names);
    }
    names.iter().any(|name| name == "arguments")
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
    let active_with_envs = parent.active_with_envs.clone();
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(function_is_strict)
        .with_arrow()
        .with_module_url(parent.module_url.clone());
    // Arrows resolve `super` lexically — inherit the statics-side
    // home flag from the enclosing context.
    child.super_home_static = parent.super_home_static;
    child.active_with_envs = active_with_envs;
    // Arrows have no implicit `arguments` object; the binding exists
    // only when a parameter or a body var / lexical / function
    // declaration introduces the name (drives the §19.2.1.3
    // direct-eval-in-parameter-defaults check).
    child.binds_arguments = arrow_binds_arguments(arrow);
    child.captured_names = capture::analyze_arrow(arrow);
    // §19.2.1.3 — a direct eval inside the arrow body uses the
    // arrow's own variable scope as its caller environment, so every
    // arrow-scope binding must live in a cell.
    let contains_direct_eval = capture::body_contains_direct_eval(Some(&arrow.params), &arrow.body);
    if contains_direct_eval {
        child
            .captured_names
            .extend(capture::all_own_names(Some(&arrow.params), &arrow.body));
    }
    child.contains_direct_eval = contains_direct_eval;
    child.reserve_known_own_upvalues();
    parent.push(child);
    parent.enter_scope();

    let param_count = u16::try_from(arrow.params.items.len()).expect("too many parameters");
    let length = formal_parameter_length(&arrow.params);
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
        module_url: parent.module_url.clone(),
        ..Default::default()
    });

    predeclare_formal_parameters(parent, &arrow.params, false, span)?;
    parent.in_param_init = true;
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
    parent.in_param_init = false;

    let mut direct_eval_meta: Vec<otter_bytecode::DirectEvalBinding> = Vec::new();
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
        if contains_direct_eval {
            direct_eval_meta = collect_direct_eval_bindings(parent, &[]);
        }
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
        let mut annex_blocked: std::collections::HashSet<String> = parent
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.keys().cloned())
            .collect();
        annex_blocked.insert("arguments".to_string());
        pre_declare_annex_b_functions(parent, &arrow.body.statements, &annex_blocked, span)?;
        pre_declare_var_bindings(parent, &var_names, span)?;
        let mut lex_names: Vec<(String, bool)> = Vec::new();
        hoist_lexical_names(&arrow.body.statements, &mut lex_names);
        pre_declare_lexical_bindings(parent, &lex_names, span)?;
        hoist_function_declarations(parent, &arrow.body.statements)?;
        for stmt in &arrow.body.statements {
            compile_statement(parent, stmt)?;
        }
        if contains_direct_eval {
            direct_eval_meta = collect_direct_eval_bindings(parent, &lex_names);
        }
        parent.emit(Op::ReturnUndefined, vec![], span);
    }
    parent.exit_scope();

    let child = parent.pop();
    if child.register_overflow {
        return Err(CompileError::Unsupported {
            node: "function body exhausts the 65535-register window".to_string(),
            span,
        });
    }

    let captures = child.parent_captures.clone();
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = child.scratch_window();
    slot.param_count = param_count;
    slot.length = length;
    slot.has_rest = has_rest;
    slot.is_async = arrow.r#async;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.is_arrow = true;
    slot.direct_eval_bindings = direct_eval_meta;
    slot.contains_direct_eval = contains_direct_eval;
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
    let operands = make_closure_operands(dst, function_const, captures, span)?;
    cx.emit(Op::MakeClosure, operands, span);
    Ok(())
}

fn make_closure_operands(
    dst: u16,
    function_const: u32,
    captures: &[u32],
    span: (u32, u32),
) -> Result<Vec<Operand>, CompileError> {
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
    Ok(operands)
}

/// §19.2.1.1 step ~6 — a direct eval inherits the caller's
/// PrivateEnvironment. Force a capture of every enclosing class's
/// private-name (and brand) cells so they ride the direct-eval
/// binding table into the eval frame. Called by every
/// function-finalization path that collects direct-eval bindings
/// (ordinary functions, synthetic and user class constructors).
pub(crate) fn capture_private_environment_for_eval(cx: &mut Compiler) {
    if cx.private_namespaces.is_empty() {
        return;
    }
    let pairs: Vec<(u32, Vec<String>)> = cx
        .private_namespaces
        .iter()
        .copied()
        .zip(cx.class_private_names.iter().cloned())
        .map(|(ns, names)| (ns, names.into_iter().collect()))
        .collect();
    for (ns, names) in pairs {
        for name in names {
            let binding = format!("__privsym_{ns}_{name}");
            let _ = cx.resolve_capture(&binding);
        }
        let brand = format!("__privbrand_{ns}");
        let _ = cx.resolve_capture(&brand);
    }
}

/// Companion to [`capture_private_environment_for_eval`] — a direct
/// eval whose call site has a [[HomeObject]] also needs the
/// synthetic super bindings spliced into its frame.
pub(crate) fn capture_super_bindings_for_eval(cx: &mut Compiler) {
    let _ = cx.resolve_capture(crate::class::SUPER_HOME_NAME);
    let _ = cx.resolve_capture(crate::class::SUPER_STATIC_HOME_NAME);
    let _ = cx.resolve_capture(crate::class::SUPER_CTOR_NAME);
}

/// §10.2.11 / §15.2.1 — it is a Syntax Error if any element of the
/// BoundNames of FormalParameters also occurs in the
/// LexicallyDeclaredNames of the function body.
pub(crate) fn validate_no_param_lexical_conflict(
    params: &oxc_ast::ast::FormalParameters<'_>,
    lex_names: &[(String, bool)],
    span: (u32, u32),
) -> Result<(), CompileError> {
    if lex_names.is_empty() {
        return Ok(());
    }
    let param_names: std::collections::HashSet<String> =
        formal_parameter_bound_names(params).into_iter().collect();
    for (name, _) in lex_names {
        if param_names.contains(name) {
            let message = format!(
                "SyntaxError: lexical declaration `{name}` shadows a formal parameter (§15.2.1)"
            );
            return Err(CompileError::Syntax {
                messages: vec![message.clone()],
                diagnostics: vec![crate::SyntaxDiagnostic {
                    code: "PARAM_LEXICAL_CONFLICT".to_string(),
                    message,
                    range: Some(span),
                    help: None,
                }],
            });
        }
    }
    Ok(())
}
