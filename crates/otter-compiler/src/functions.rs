//! Function, arrow, and callable lowering.
//!
//! One lowering serves ordinary functions, methods, class constructors, and
//! arrows. A function's scopes follow FunctionDeclarationInstantiation
//! (§10.2.11), outermost first:
//!
//! - `FunctionName` — a named function expression's immutable self binding,
//!   initialized from `LoadSelf`;
//! - `Callee` — only for a sloppy function whose parameter list has
//!   expressions and a direct eval: the extension anchor for that eval's
//!   `var`s, outside the parameters;
//! - `Params` — formals and `arguments` when the parameter list has
//!   expressions (their slots start in the TDZ);
//! - `Body` — the VariableEnvironment: `var`s, functions, and top-level
//!   lexicals; with a simple parameter list the formals live here too. A
//!   sloppy function whose own code calls eval anchors the extension here.
//!
//! Each scope gets a context only when it owns a slot or anchors an
//! extension. A derived constructor whose `this` an arrow, an arrow
//! `super()`, or a direct eval observes keeps `this` in a `DerivedThis` slot
//! of its first own scope.
//!
//! # Contents
//! - [`compile_function_full`] / [`compile_arrow_function`] — lower one
//!   function into the module's function table.
//! - [`compile_function_impl`] — the shared lowering, with optional class
//!   instance-field injection for constructors.
//! - [`ClosureRecord`] / [`emit_make_callable`] — materialize the callable
//!   over the creator's innermost context.
//! - [`finish_function`] — write a finished frame into its record.
//!
//! # Invariants
//! - Nested functions are registered in the shared module builder.
//! - Every `CreateContext` of the prologue precedes `GeneratorStart`.
//! - A closure captures exactly the context that was innermost when its
//!   body was compiled, which is the chain its static depths assume.
//! - An implicit undefined return is omitted when the final source statement
//!   already returns, so the authoritative CFG contains no dead tail block.
//!
//! # See also
//! - `params` and `function_context`

use crate::function_context::DerivedThisSlot;
use crate::scope::CtxReg;
use crate::*;
use otter_bytecode::{ScopeFlags, ScopeKind, SlotKind};

/// A compiled function plus the context its closure must capture.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClosureRecord {
    pub(crate) function_id: u32,
    /// The creator's innermost context when the body was compiled.
    pub(crate) ctx: CtxReg,
    /// The body reads its closure context.
    pub(crate) needs_context: bool,
    pub(crate) is_arrow: bool,
}

/// Class instance fields a constructor initializes inline.
pub(crate) struct FieldInjection<'r, 'a> {
    pub(crate) fields: &'r [&'r oxc_ast::ast::PropertyDefinition<'a>],
    pub(crate) is_derived: bool,
}

/// Everything [`compile_function_impl`] needs about one function.
pub(crate) struct FunctionSpec<'r, 'a> {
    pub(crate) name: &'r str,
    pub(crate) params: &'r oxc_ast::ast::FormalParameters<'a>,
    pub(crate) body: Option<&'r oxc_ast::ast::FunctionBody<'a>>,
    pub(crate) span: (u32, u32),
    pub(crate) is_async: bool,
    pub(crate) is_generator: bool,
    pub(crate) force_strict: bool,
    pub(crate) is_arrow: bool,
    /// Arrow with a concise expression body.
    pub(crate) arrow_expression: bool,
    /// Named function expression: bind the self name (§15.2.5 funcEnv).
    pub(crate) nfe_self: bool,
    pub(crate) fields: Option<FieldInjection<'r, 'a>>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_function_full(
    parent: &mut Compiler,
    name: &str,
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &Option<oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody<'_>>>,
    span: (u32, u32),
    is_async: bool,
    is_generator: bool,
    force_strict: bool,
) -> Result<ClosureRecord, CompileError> {
    compile_function_impl(
        parent,
        FunctionSpec {
            name,
            params,
            body: body.as_deref(),
            span,
            is_async,
            is_generator,
            force_strict,
            is_arrow: false,
            arrow_expression: false,
            nfe_self: false,
            fields: None,
        },
    )
}

/// Compile a named function expression, binding its self name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_named_function_expression(
    parent: &mut Compiler,
    name: &str,
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &Option<oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody<'_>>>,
    span: (u32, u32),
    is_async: bool,
    is_generator: bool,
) -> Result<ClosureRecord, CompileError> {
    compile_function_impl(
        parent,
        FunctionSpec {
            name,
            params,
            body: body.as_deref(),
            span,
            is_async,
            is_generator,
            force_strict: false,
            is_arrow: false,
            arrow_expression: false,
            nfe_self: true,
            fields: None,
        },
    )
}

/// Compile an arrow function. `() => expr` lowers to one
/// `ReturnValue(expr)`; a block body lowers like any function body.
pub(crate) fn compile_arrow_function(
    parent: &mut Compiler,
    arrow: &oxc_ast::ast::ArrowFunctionExpression<'_>,
    span: (u32, u32),
) -> Result<ClosureRecord, CompileError> {
    compile_function_impl(
        parent,
        FunctionSpec {
            name: "<arrow>",
            params: &arrow.params,
            body: Some(&arrow.body),
            span,
            is_async: arrow.r#async,
            is_generator: false,
            force_strict: false,
            is_arrow: true,
            arrow_expression: arrow.expression,
            nfe_self: false,
            fields: None,
        },
    )
}

/// `true` when an `arguments` binding already exists in an arrow's
/// variable environment *during parameter instantiation* — i.e. a
/// formal parameter (or rest pattern) declares the name.
fn arrow_binds_arguments(params: &oxc_ast::ast::FormalParameters<'_>) -> bool {
    let mut names: Vec<String> = Vec::new();
    for param in &params.items {
        collect_pattern_var_names(&param.pattern, &mut names);
    }
    if let Some(rest) = &params.rest {
        collect_pattern_var_names(&rest.rest.argument, &mut names);
    }
    names.iter().any(|name| name == "arguments")
}

pub(crate) fn compile_function_impl(
    parent: &mut Compiler,
    spec: FunctionSpec<'_, '_>,
) -> Result<ClosureRecord, CompileError> {
    let FunctionSpec {
        name,
        params,
        body,
        span,
        is_async,
        is_generator,
        force_strict,
        is_arrow,
        arrow_expression,
        nfe_self,
        fields,
    } = spec;
    let is_async_generator = is_async && is_generator;
    let is_method = std::mem::take(&mut parent.next_fn_is_method);
    let hinted_home = std::mem::take(&mut parent.next_fn_has_home);
    let hinted_derived_ctor = std::mem::take(&mut parent.next_fn_derived_ctor);
    let static_home = std::mem::take(&mut parent.next_fn_static_home);
    let source_text_span = std::mem::take(&mut parent.next_fn_source_text_span);
    let module = Rc::clone(&parent.top_mut().module);
    let body_has_strict_directive = body.is_some_and(|b| b.has_use_strict_directive());
    let function_is_strict = force_strict || parent.is_strict || body_has_strict_directive;
    let simple_params = formal_parameters_are_simple(params);
    let has_param_expressions = formal_parameters_contain_expression(params);
    // §15.4.1 — MethodDefinition uses UniqueFormalParameters even in
    // sloppy code; arrows never allow duplicates.
    let allow_duplicate_formals = !function_is_strict && simple_params && !is_method && !is_arrow;
    let fields_contain_eval = fields.as_ref().is_some_and(|injection| {
        injection.fields.iter().any(|field| {
            field
                .value
                .as_ref()
                .is_some_and(capture::expression_contains_direct_eval)
        })
    });
    // Any direct eval inside (nested functions included) can read every
    // binding it can see, so every own binding becomes a slot.
    let unit = Rc::clone(&parent.capture);
    let facts = body.map(|b| unit.function(b));
    let contains_direct_eval =
        facts.is_some_and(|facts| unit.contains_eval(facts)) || fields_contain_eval;
    // A sloppy direct eval in the function's OWN code creates `var`s in the
    // variable environment: that scope anchors the eval extension.
    let params_eval = has_param_expressions
        && !function_is_strict
        && facts.is_some_and(capture::ScopeFacts::params_eval);
    let body_eval = !function_is_strict && facts.is_some_and(capture::ScopeFacts::body_eval);
    let legacy_arguments_observable = !is_arrow
        && parent.dot_arguments_observed
        && !function_is_strict
        && !is_method
        && !is_async
        && !is_generator
        && body.is_some_and(|b| capture::function_body_reads_dot_arguments(params, b));
    // §10.2.11 step 18 — a top-level lexical `arguments` (simple
    // parameter list) replaces the arguments object.
    let lexical_arguments = !has_param_expressions
        && body.is_some_and(|b| {
            let mut names: Vec<(String, bool)> = Vec::new();
            hoist_lexical_names(&b.statements, &mut names);
            names.iter().any(|(name, _)| name == "arguments")
        });
    let body_needs_arguments_object = !is_arrow
        && !lexical_arguments
        && (facts.is_some_and(capture::ScopeFacts::uses_arguments) || contains_direct_eval);
    let needs_arguments = body_needs_arguments_object || legacy_arguments_observable;
    let uses_mapped_arguments = body_needs_arguments_object && !function_is_strict && simple_params;
    // A mapped object aliases the formals only where the aliasing can be
    // observed: the object escapes or is written, a formal is assigned, or a
    // direct eval or a legacy `f.arguments` read can reach either. Otherwise
    // its elements always equal the formals and the formals stay plain.
    let aliased_formals = uses_mapped_arguments
        && (contains_direct_eval
            || parent.dot_arguments_observed
            || facts.is_none_or(|facts| {
                facts.arguments_escapes()
                    || formal_parameter_bound_names(params)
                        .iter()
                        .any(|name| unit.assigned(facts, name))
            }));
    let arguments_forward_only = body_needs_arguments_object
        && !contains_direct_eval
        && !crate::hoist::function_declares_name(params, body, "arguments")
        && crate::hoist::arguments_uses_are_forwarded(params, body);
    validate_formal_parameter_names(params, function_is_strict, allow_duplicate_formals, span)?;
    let derived_this = hinted_derived_ctor && capture::derived_this_observed(params, body);

    let active_with_envs = parent.active_with_envs.clone();
    let mut child = FunctionContext::new(Rc::clone(&module))
        .with_strict(function_is_strict)
        .with_module_url(parent.module_url.clone());
    if is_arrow {
        child = child.with_arrow();
        // Arrows resolve `super` lexically.
        child.super_home_static = parent.super_home_static;
        child.binds_arguments = arrow_binds_arguments(params);
    } else {
        child.super_home_static = static_home;
        child.has_home_object = is_method || hinted_home;
        child.is_derived_ctor = hinted_derived_ctor;
        // §10.2.11 — every non-arrow function's variable environment
        // binds `arguments`.
        child.binds_arguments = true;
    }
    child.dot_arguments_observed = parent.dot_arguments_observed;
    child.active_with_envs = active_with_envs;
    child.is_async_generator = is_async_generator;
    child.proper_tail_calls = !is_async && !is_generator;
    let formal_names: Vec<String> = formal_parameter_bound_names(params);
    let mut self_name_referenced = false;
    if let Some(facts) = facts {
        child.captured_names = unit.captured(facts, true);
        if is_arrow {
            child.captured_names.remove("arguments");
        }
        let self_name_in_inner = contains_direct_eval || unit.inner_references(facts, name);
        self_name_referenced = contains_direct_eval || unit.references(facts, name);
        if nfe_self && self_name_in_inner {
            child.captured_names.insert(name.to_string());
        }
        // A nested closure referencing `arguments` (in a parameter default
        // or the body) reads the object through a slot.
        if body_needs_arguments_object && unit.inner_references(facts, "arguments") {
            child.captured_names.insert("arguments".to_string());
        }
        if contains_direct_eval {
            child.captured_names.extend(unit.own_names(facts, true));
            if is_arrow {
                child.captured_names.remove("arguments");
                if child.binds_arguments {
                    child.captured_names.insert("arguments".to_string());
                }
            }
        }
    }
    if fields_contain_eval {
        child.captured_names.insert("arguments".to_string());
    }
    child.contains_direct_eval = contains_direct_eval;
    child.arguments_forward_only = arguments_forward_only;
    if aliased_formals {
        child.mapped_argument_names = simple_formal_names(params).into_iter().collect();
    }

    let parent_ctx = parent.innermost_ctx();
    child.closure_context_empty = parent_ctx == CtxReg::Closure && parent.closure_context_empty;
    parent.push(child);
    // A non-arrow function owns its `new.target` — the field-initializer
    // signal does not propagate into its body.
    let saved_field_init = parent.in_field_initializer;
    if !is_arrow {
        parent.in_field_initializer = false;
    }

    let param_count = u16::try_from(params.items.len()).expect("too many parameters");
    let length = formal_parameter_length(params);
    parent.scratch = param_count;
    let has_rest = params.rest.is_some();

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        source_text_span,
        is_strict: function_is_strict,
        module_url: parent.module_url.clone(),
        ..Default::default()
    });

    let result = compile_function_scopes(
        parent,
        FunctionScopes {
            name,
            params,
            body,
            span,
            is_generator,
            is_arrow,
            arrow_expression,
            function_is_strict,
            has_param_expressions,
            allow_duplicate_formals,
            self_binding: nfe_self
                && self_name_referenced
                && !name.is_empty()
                && !formal_names.iter().any(|formal| formal == name),
            params_eval,
            body_eval,
            derived_this,
            body_needs_arguments_object,
            arguments_forward_only,
            aliased_formals,
            fields,
        },
    );
    parent.in_field_initializer = saved_field_init;
    let mapped_argument_bindings = match result {
        Ok(bindings) => bindings,
        Err(error) => {
            parent.pop();
            return Err(error);
        }
    };

    let mut child = parent.pop();
    // No aliased formal, eval or suspension can observe hidden identity in
    // this admitted family, and without exception handlers every edge is a
    // normal successor. Alias/escape proof uses lowered register flow.
    if needs_arguments
        && mapped_argument_bindings.is_empty()
        && !contains_direct_eval
        && !is_async
        && !is_generator
        && !is_async_generator
        && child.handlers.is_empty()
        && let Some(plan) = crate::arguments_elision::analyze(
            &child.code,
            &module.borrow().constants,
            child.scratch_window(),
            &child.closure_ctx_patches,
        )
    {
        crate::arguments_elision::lower(&mut child.code, &plan);
    }
    let record = finish_function(parent, &mut child, function_id, span, |slot| {
        slot.param_count = param_count;
        slot.length = length;
        slot.has_rest = has_rest;
        slot.is_async = is_async;
        slot.is_generator = is_generator;
        slot.is_async_generator = is_async_generator;
        slot.is_method = is_method;
        slot.is_arrow = is_arrow;
        slot.is_derived_constructor = hinted_derived_ctor;
        slot.needs_arguments = needs_arguments;
        slot.asm_module = body.is_some_and(|body| {
            body.directives
                .iter()
                .any(|directive| directive.directive.as_str() == "use asm")
        });
        slot.uses_arguments_callee =
            body_needs_arguments_object && crate::hoist::body_uses_arguments_callee(params, body);
        slot.arguments_object_kind = if uses_mapped_arguments {
            ArgumentsObjectKind::Mapped
        } else {
            ArgumentsObjectKind::Unmapped
        };
        slot.mapped_argument_bindings = mapped_argument_bindings;
        slot.contains_direct_eval = contains_direct_eval;
    })?;
    Ok(ClosureRecord {
        function_id,
        ctx: parent_ctx,
        needs_context: record,
        is_arrow,
    })
}

struct FunctionScopes<'r, 'a> {
    name: &'r str,
    params: &'r oxc_ast::ast::FormalParameters<'a>,
    body: Option<&'r oxc_ast::ast::FunctionBody<'a>>,
    span: (u32, u32),
    is_generator: bool,
    is_arrow: bool,
    arrow_expression: bool,
    function_is_strict: bool,
    has_param_expressions: bool,
    allow_duplicate_formals: bool,
    self_binding: bool,
    params_eval: bool,
    body_eval: bool,
    derived_this: bool,
    body_needs_arguments_object: bool,
    arguments_forward_only: bool,
    aliased_formals: bool,
    fields: Option<FieldInjection<'r, 'a>>,
}

/// Lower the function's scopes and body into the frame on top of the
/// stack. Returns the mapped-arguments table.
fn compile_function_scopes(
    parent: &mut Compiler,
    f: FunctionScopes<'_, '_>,
) -> Result<Vec<MappedArgumentBinding>, CompileError> {
    let span = f.span;
    let strict = f.function_is_strict;
    // §15.2.5 — a named function expression's funcEnv holds its own name,
    // outside every other scope of the function.
    if f.self_binding {
        parent.enter_scope_with_flags(
            ScopeKind::FunctionName,
            ScopeFlags {
                strict,
                var_scope: false,
                has_extension: false,
            },
        );
        let storage = parent.declare_binding(f.name, SlotKind::FnSelfName, span)?;
        parent.mark_fn_self_name(f.name);
        match storage {
            BindingStorage::Register { reg } => {
                parent.emit(Op::LoadSelf, [Operand::Register(reg)], span);
            }
            BindingStorage::Slot { .. } => {
                let tmp = parent.alloc_scratch();
                parent.emit(Op::LoadSelf, [Operand::Register(tmp)], span);
                parent.emit_store_storage(tmp, storage, span);
            }
        }
        parent.mark_initialized(f.name);
    }
    // §10.2.11 step 20 — a sloppy parameter list with expressions keeps a
    // separate callee environment that receives a parameter eval's `var`s.
    if f.params_eval {
        parent.enter_scope_with_flags(
            ScopeKind::Callee,
            ScopeFlags {
                strict: false,
                var_scope: true,
                has_extension: true,
            },
        );
        parent.ensure_innermost_context(span)?;
    }
    let param_scope_kind = if f.has_param_expressions {
        ScopeKind::Params
    } else {
        ScopeKind::Body
    };
    let simple_body_extension = !f.has_param_expressions && f.body_eval;
    parent.enter_scope_with_flags(
        param_scope_kind,
        ScopeFlags {
            strict,
            var_scope: !f.has_param_expressions,
            has_extension: simple_body_extension,
        },
    );
    let param_scope = parent.scopes.len() - 1;
    parent.top_mut().var_scope = param_scope;
    if simple_body_extension {
        parent.ensure_innermost_context(span)?;
    }
    if f.derived_this {
        let storage = parent.declare_forced_slot("this", SlotKind::DerivedThis, span)?;
        let BindingStorage::Slot {
            ctx: CtxReg::Reg(ctx),
            slot,
        } = storage
        else {
            unreachable!("a function's own scope context is a register");
        };
        parent.mark_initialized("this");
        parent.top_mut().derived_this = Some(DerivedThisSlot { ctx, slot });
    }
    // §15.7.10 — a base class runs field initializers from [[Construct]]
    // before the constructor binds its parameters: they never observe the
    // constructor's parameter bindings.
    if let Some(injection) = &f.fields
        && !injection.is_derived
    {
        crate::class::emit_instance_field_inits(parent, injection.fields)?;
    }

    predeclare_formal_parameters(
        parent,
        f.params,
        f.allow_duplicate_formals,
        f.has_param_expressions,
        span,
    )?;
    // §10.2.11 step 22 — bind `arguments` BEFORE the formals are
    // initialized, so a parameter default can read it.
    if f.body_needs_arguments_object
        && !f.arguments_forward_only
        && parent.lookup_in_current_scope("arguments").is_none()
    {
        let storage = parent.declare_binding("arguments", SlotKind::Arguments, span)?;
        let tmp = parent.alloc_scratch();
        let ctx_operand = match parent.scopes[param_scope].context.map(|ctx| ctx.reg) {
            Some(CtxReg::Reg(reg)) if f.aliased_formals => reg,
            _ => tmp,
        };
        parent.emit(
            Op::CollectArguments,
            [Operand::Register(tmp), Operand::Register(ctx_operand)],
            span,
        );
        parent.emit_store_storage(tmp, storage, span);
        parent.mark_initialized("arguments");
    }
    parent.in_param_init = true;
    for (ordinal, param) in f.params.items.iter().enumerate() {
        compile_formal_parameter(
            parent,
            ordinal as u16,
            &param.pattern,
            param.initializer.as_deref(),
            span,
            f.allow_duplicate_formals,
        )?;
    }
    if let Some(rest) = &f.params.rest {
        compile_rest_parameter(parent, &rest.rest.argument, span)?;
    }
    parent.in_param_init = false;
    crate::type_hints::annotate_formal_parameters(parent, f.params);
    let mapped_argument_bindings = if f.aliased_formals {
        mapped_formal_parameter_bindings(parent, f.params)
    } else {
        Vec::new()
    };
    // `CallForwardArguments` names the mapped formals' context register.
    parent.top_mut().mapped_arguments_ctx =
        mapped_argument_bindings
            .iter()
            .find_map(|binding| match binding.storage {
                ArgumentBindingStorage::Context { reg, .. } => Some(reg),
                ArgumentBindingStorage::Register { .. } => None,
            });

    if f.is_arrow && f.arrow_expression {
        let body = f.body.expect("arrow body");
        let stmt = body.statements.first().ok_or(CompileError::Unsupported {
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
        // §15.10.2 — a strict non-async concise body is in tail position.
        if parent.is_strict && parent.proper_tail_calls {
            crate::statements::compile_tail_return(parent, &es.expression, inner_span)?;
        } else {
            let reg = compile_expr(parent, &es.expression, inner_span)?;
            parent.emit(Op::ReturnValue, [Operand::Register(reg)], inner_span);
        }
        return Ok(mapped_argument_bindings);
    }

    // §10.2.11 step 28 — with parameter expressions the body owns a
    // separate variable environment.
    if f.has_param_expressions {
        parent.enter_scope_with_flags(
            ScopeKind::Body,
            ScopeFlags {
                strict,
                var_scope: true,
                has_extension: f.body_eval,
            },
        );
        let body_scope = parent.scopes.len() - 1;
        parent.top_mut().var_scope = body_scope;
        if f.body_eval {
            parent.ensure_innermost_context(span)?;
        }
    }
    let mut ends_with_return = false;
    if let Some(body) = f.body {
        let mut var_names: Vec<String> = Vec::new();
        hoist_var_names(&body.statements, &mut var_names);
        // §B.3.3.1 — parameter names and "arguments" block the sloppy
        // block-level function var-scope extension.
        let mut annex_blocked: std::collections::HashSet<String> =
            formal_parameter_bound_names(f.params).into_iter().collect();
        annex_blocked.insert("arguments".to_string());
        pre_declare_annex_b_functions(parent, &body.statements, &annex_blocked, span)?;
        pre_declare_var_bindings(parent, &var_names, span)?;
        // §10.2.11 step 28.f — with parameter expressions, a body `var`
        // naming a parameter-environment binding starts with that
        // binding's current value.
        if f.has_param_expressions {
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for name in &var_names {
                if !seen.insert(name.as_str()) {
                    continue;
                }
                let Some(param_info) = parent.scopes[param_scope].bindings.get(name).copied()
                else {
                    continue;
                };
                let Some(body_info) = parent.lookup_in_current_scope(name) else {
                    continue;
                };
                let tmp = parent.alloc_scratch();
                parent.emit_load_storage(tmp, param_info.storage, span);
                parent.emit_store_storage(tmp, body_info.storage, span);
            }
        }
        let mut lex_names: Vec<(String, bool)> = Vec::new();
        hoist_lexical_names(&body.statements, &mut lex_names);
        validate_no_param_lexical_conflict(f.params, &lex_names, span)?;
        pre_declare_lexical_bindings(parent, &lex_names, span)?;
        hoist_function_declarations(parent, &body.statements)?;
        if f.is_generator {
            parent.emit(Op::GeneratorStart, vec![], span);
        }
        let derived_fields = f.fields.as_ref().filter(|injection| injection.is_derived);
        let mut fields_emitted = derived_fields.is_none();
        for stmt in &body.statements {
            compile_discarded_statement(parent, stmt)?;
            // Derived classes initialize fields once the constructor's
            // statement-level `super(...)` returns.
            if !fields_emitted && crate::class::is_top_level_super_call(stmt) {
                crate::class::emit_instance_field_inits(
                    parent,
                    derived_fields.expect("derived field injection").fields,
                )?;
                fields_emitted = true;
            }
        }
        if !fields_emitted {
            crate::class::emit_instance_field_inits(
                parent,
                derived_fields.expect("derived field injection").fields,
            )?;
        }
        ends_with_return = matches!(body.statements.last(), Some(Statement::ReturnStatement(_)));
    } else if let Some(injection) = &f.fields
        && injection.is_derived
    {
        crate::class::emit_instance_field_inits(parent, injection.fields)?;
    }
    if !ends_with_return {
        parent.emit(Op::ReturnUndefined, vec![], span);
    }
    Ok(mapped_argument_bindings)
}

/// Finalize a popped frame into its reserved function record. `fill` sets
/// the shape fields the caller owns. Returns whether the body reads its
/// closure context.
pub(crate) fn finish_function(
    parent: &mut Compiler,
    child: &mut FunctionContext,
    function_id: u32,
    span: (u32, u32),
    fill: impl FnOnce(&mut Function),
) -> Result<bool, CompileError> {
    let finished = child.finish_code(span);
    if child.register_overflow {
        return Err(CompileError::Unsupported {
            node: "function body exhausts the 65535-register window".to_string(),
            span,
        });
    }
    let module = Rc::clone(&parent.top_mut().module);
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = finished.scratch;
    slot.scopes = finished.scopes;
    slot.number_hint_sites = finished.number_hint_sites;
    slot.code = finished.code;
    slot.handlers = finished.handlers;
    slot.spans = otter_bytecode::SpanTable::new(&finished.spans);
    fill(slot);
    drop(module_mut);
    parent.take_class_hint_sites(function_id, finished.class_hint_sites);
    Ok(finished.uses_closure_context)
}

/// Materialize a callable for `record` into `dst`: a closure over the
/// creator's context when the body reads it, a context-free function
/// otherwise. Arrows always go through `MakeClosure` so the VM snapshots
/// the creator's `this` / `new.target`.
pub(crate) fn emit_make_callable(
    cx: &mut Compiler,
    dst: u16,
    record: &ClosureRecord,
    span: (u32, u32),
) {
    emit_make_callable_as(cx, dst, record, record.is_arrow, span);
}

/// [`emit_make_callable`] that always yields a closure object (methods).
pub(crate) fn emit_make_callable_object(
    cx: &mut Compiler,
    dst: u16,
    record: &ClosureRecord,
    span: (u32, u32),
) {
    emit_make_callable_as(cx, dst, record, true, span);
}

fn emit_make_callable_as(
    cx: &mut Compiler,
    dst: u16,
    record: &ClosureRecord,
    closure_object: bool,
    span: (u32, u32),
) {
    let function_const = cx.intern_function_id(record.function_id);
    // A closure context that is statically `undefined` needs no operand.
    let context_empty = record.ctx == CtxReg::Closure && cx.closure_context_empty;
    if record.needs_context && !context_empty {
        cx.emit_ctx(
            Op::MakeClosure,
            vec![
                Operand::Register(dst),
                Operand::ConstIndex(function_const),
                Operand::Register(0),
            ],
            2,
            record.ctx,
            span,
        );
    } else if closure_object {
        // No context to capture: close over `undefined`.
        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
        cx.emit(
            Op::MakeClosure,
            [
                Operand::Register(dst),
                Operand::ConstIndex(function_const),
                Operand::Register(dst),
            ],
            span,
        );
    } else {
        cx.emit(
            Op::MakeFunction,
            [Operand::Register(dst), Operand::ConstIndex(function_const)],
            span,
        );
    }
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
