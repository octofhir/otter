//! Public and internal entry points for script and module compilation.
//!
//! # Contents
//! - source parsing entry points ([`compile_script_source`],
//!   [`compile_eval_source`], [`compile_module_program`])
//! - borrowed AST lowering of script, eval, and module bodies
//! - direct-eval variable-environment targets and step-3 conflict checks
//! - module metadata assembly
//! - export declaration lowering
//!
//! # Invariants
//! - The first function record is the entry function for the produced bytecode module.
//! - A direct eval's `<main>` reads its caller's context chain through
//!   `LoadClosureContext`; caller bindings resolve by static hop counts over
//!   the [`EvalCallerChain`] descriptors, and a sloppy body's `var`s bind in
//!   the caller's variable scope (a static slot or its eval extension).
//! - `<module-init>`'s scope 0 is the Module scope: descriptor 0, allocated
//!   by the runtime and shared by the link and evaluation invocations.
//!
//! # See also
//! - `errors` and `module_state`

use crate::*;
use otter_bytecode::EvalCallerChain;

/// Compile source text through a single OXC parse.
///
/// This is the runtime hot path for scripts that do not need to inspect the AST
/// before lowering. Use [`compile_script_program`] when a caller already has a
/// borrowed OXC program from [`otter_syntax::with_program`].
///
/// # Errors
/// Returns [`CompileError`] when parsing fails or the AST contains constructs
/// outside the foundation subset.
pub fn compile_script_source(
    source: &str,
    kind: SyntaxSourceKind,
    module_specifier: &str,
) -> Result<BytecodeModule, CompileError> {
    compile_script_source_with_forced_strict(source, kind, module_specifier, false)
}

/// Compile a classic-script source whose embedder permits top-level
/// `await` (REPL-style snippet APIs). Parses with the Module goal so
/// top-level `await` suspends, while lowering stays on the script
/// pipeline: the produced `<main>` is async when the body awaits and
/// the runtime drives it through its async entry promise.
///
/// # Errors
/// Returns [`CompileError`] when parsing fails or lowering rejects the AST.
pub fn compile_script_source_with_top_level_await(
    source: &str,
    kind: SyntaxSourceKind,
    module_specifier: &str,
) -> Result<BytecodeModule, CompileError> {
    with_program(source, kind, |program| {
        compile_program(program, kind, module_specifier, false)
    })
    .map_err(CompileError::from)?
}

/// Compile source text with an optional inherited strict-mode
/// override. Direct eval uses this to model ECMA-262's caller
/// strictness inheritance without rewriting source text.
///
/// # Errors
/// Returns [`CompileError`] when parsing fails or lowering rejects the AST.
pub fn compile_script_source_with_forced_strict(
    source: &str,
    kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
) -> Result<BytecodeModule, CompileError> {
    // §16.1 Script goal: `await` stays a plain identifier and
    // `import` / `export` declarations are early syntax errors.
    otter_syntax::with_program_goal(source, kind, otter_syntax::SourceGoal::Script, |program| {
        compile_program(program, kind, module_specifier, force_strict)
    })
    .map_err(CompileError::from)?
}

/// Compile an `eval` / `new Function` body. Differs from script
/// compilation in two details: a *strict* eval body gets its own
/// variable environment (§19.2.1.1), so top-level `var` / `function`
/// declarations don't mirror onto the global object; and when the
/// direct-eval call site's variable environment binds `arguments`
/// (`forbid_var_arguments`), a sloppy body var-declaring `arguments`
/// is an early SyntaxError (§19.2.1.3 EvalDeclarationInstantiation).
///
/// `caller_chain` is the context chain of a direct eval's call site:
/// `chain.scopes[0]` is the caller's innermost context, and
/// `chain.var_depth` the hop count of the variable-scope context whose
/// eval extension receives a sloppy body's `var`s (`None` = the global
/// variable environment). The produced `<main>` reads that chain through
/// `LoadClosureContext` (the runtime runs it as a closure over the call
/// site's context) and resolves caller bindings by static hop counts.
/// `None` compiles an indirect eval or a `Function` constructor body:
/// global code with no caller context.
///
/// # Errors
/// Returns [`CompileError`] when parsing fails or lowering rejects the AST.
#[allow(clippy::too_many_arguments)]
pub fn compile_eval_source(
    source: &str,
    kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
    forbid_var_arguments: bool,
    caller_chain: Option<&EvalCallerChain>,
    new_target_allowed: bool,
    in_class_field_initializer: bool,
    super_property_allowed: bool,
    super_call_allowed: bool,
    function_constructor: bool,
) -> Result<BytecodeModule, CompileError> {
    // §19.2.1.1 PerformEval parses the body with the Script goal.
    // A `super` reference parses cleanly and is rejected by our own
    // semantic pass (inner `Err`), whereas oxc rejects `new.target` at
    // parse time (outer `Err`). Flatten both into one `CompileError` so
    // the wrap-and-retry below can recover either.
    let direct: Result<BytecodeModule, CompileError> = match otter_syntax::with_program_goal(
        source,
        kind,
        otter_syntax::SourceGoal::Script,
        |program| {
            compile_eval_parts(
                ProgramParts::of(program),
                kind,
                module_specifier,
                force_strict,
                forbid_var_arguments,
                caller_chain,
                new_target_allowed,
                in_class_field_initializer,
                super_property_allowed,
                super_call_allowed,
                function_constructor,
            )
        },
    ) {
        Ok(inner) => inner,
        Err(parse_err) => Err(CompileError::from(parse_err)),
    };
    match direct {
        Err(CompileError::Syntax { ref messages, .. })
            if ((super_property_allowed || super_call_allowed)
                && messages.iter().any(|m| m.contains("super")))
                || (new_target_allowed && messages.iter().any(|m| m.contains("new.target"))) =>
        {
            // §19.2.1.1 — `super` references and `new.target` are legal
            // in eval code whose call site supplies them (methods, field
            // initializers, function bodies). oxc has no allow-super /
            // allow-new-target parse switch for Script goal, so re-parse
            // the body inside a synthetic concise method (which is a
            // non-arrow function with a [[HomeObject]], so both parse)
            // and lower its statements through the ordinary eval
            // pipeline. The wrapper is only a parse vehicle; the
            // extracted statements are compiled with the original eval
            // flags, so runtime binding of `super` / `new.target`
            // resolves against the real caller.
            // A `super()` call only parses inside a derived-class
            // constructor, so a super-call-capable eval re-parses in
            // that shape; everything else uses the concise-method
            // wrapper (which supplies a [[HomeObject]] for `super.x`).
            let wrapped = if super_call_allowed {
                format!("(class extends Object {{ constructor() {{\n{source}\n}} }});")
            } else {
                format!("({{ __otter_eval__() {{\n{source}\n}} }});")
            };
            otter_syntax::with_program_goal(
                &wrapped,
                kind,
                otter_syntax::SourceGoal::Script,
                |program| {
                    let body = if super_call_allowed {
                        extract_wrapped_eval_ctor_body(program)?
                    } else {
                        extract_wrapped_eval_body(program)?
                    };
                    if statements_contain_top_level_return(&body.statements) {
                        return Err(CompileError::Unsupported {
                            node: "SyntaxError: return is not allowed in eval code".to_string(),
                            span: (program.span.start, program.span.end),
                        });
                    }
                    compile_eval_parts(
                        ProgramParts {
                            body: &body.statements,
                            directives: &body.directives,
                            span: (program.span.start, program.span.end),
                            strict_directive: body.has_use_strict_directive(),
                            source_text: program.source_text,
                        },
                        kind,
                        module_specifier,
                        force_strict,
                        forbid_var_arguments,
                        caller_chain,
                        new_target_allowed,
                        in_class_field_initializer,
                        super_property_allowed,
                        super_call_allowed,
                        function_constructor,
                    )
                },
            )
            .map_err(CompileError::from)?
        }
        other => other,
    }
}

/// Locate the synthetic derived-ctor wrapper's body:
/// `(class extends Object { constructor() { ... } });`.
fn extract_wrapped_eval_ctor_body<'a, 'b>(
    program: &'b Program<'a>,
) -> Result<&'b oxc_ast::ast::FunctionBody<'a>, CompileError> {
    use oxc_ast::ast::{ClassElement, Expression, Statement};
    let err = || CompileError::Unsupported {
        node: "internal: eval super-wrapper shape mismatch".to_string(),
        span: (program.span.start, program.span.end),
    };
    let Some(Statement::ExpressionStatement(es)) = program.body.first() else {
        return Err(err());
    };
    let mut expr = &es.expression;
    while let Expression::ParenthesizedExpression(p) = expr {
        expr = &p.expression;
    }
    let Expression::ClassExpression(class) = expr else {
        return Err(err());
    };
    let Some(ClassElement::MethodDefinition(ctor)) = class.body.body.first() else {
        return Err(err());
    };
    ctor.value.body.as_deref().ok_or_else(err)
}

/// Locate the synthetic wrapper's method body:
/// `({ __otter_eval__() { ... } });`.
fn extract_wrapped_eval_body<'a, 'b>(
    program: &'b Program<'a>,
) -> Result<&'b oxc_ast::ast::FunctionBody<'a>, CompileError> {
    use oxc_ast::ast::{Expression, ObjectPropertyKind, Statement};
    let err = || CompileError::Unsupported {
        node: "internal: eval super-wrapper shape mismatch".to_string(),
        span: (program.span.start, program.span.end),
    };
    let Some(Statement::ExpressionStatement(es)) = program.body.first() else {
        return Err(err());
    };
    let mut expr = &es.expression;
    while let Expression::ParenthesizedExpression(p) = expr {
        expr = &p.expression;
    }
    let Expression::ObjectExpression(obj) = expr else {
        return Err(err());
    };
    let Some(ObjectPropertyKind::ObjectProperty(prop)) = obj.properties.first() else {
        return Err(err());
    };
    let Expression::FunctionExpression(f) = &prop.value else {
        return Err(err());
    };
    f.body.as_deref().ok_or_else(err)
}

/// §19.2.1.1 — `return` stays illegal in eval code even though the
/// wrapper method would let it parse. Walks statements without
/// descending into nested functions.
/// §19.2.1.1 — `super()` CALLS stay illegal in eval code unless the
/// call site is a derived constructor (we only flag SuperProperty
/// sites today, so any direct super-call in eval is an early
/// SyntaxError). Arrows are transparent; ordinary functions open
/// their own home scope.
fn statements_contain_super_call(stmts: &[oxc_ast::ast::Statement<'_>]) -> bool {
    use oxc_ast_visit::Visit;
    #[derive(Default)]
    struct SuperCallFinder {
        found: bool,
    }
    impl<'a> Visit<'a> for SuperCallFinder {
        fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
            if matches!(it.callee, oxc_ast::ast::Expression::Super(_)) {
                self.found = true;
            }
            oxc_ast_visit::walk::walk_call_expression(self, it);
        }
        fn visit_function(
            &mut self,
            _: &oxc_ast::ast::Function<'a>,
            _: oxc_syntax::scope::ScopeFlags,
        ) {
        }
        fn visit_class_body(&mut self, _: &oxc_ast::ast::ClassBody<'a>) {}
    }
    let mut finder = SuperCallFinder::default();
    for stmt in stmts {
        finder.visit_statement(stmt);
    }
    finder.found
}

fn statements_contain_top_level_return(stmts: &[oxc_ast::ast::Statement<'_>]) -> bool {
    use oxc_ast_visit::Visit;
    #[derive(Default)]
    struct ReturnFinder {
        found: bool,
    }
    impl<'a> Visit<'a> for ReturnFinder {
        fn visit_return_statement(&mut self, _: &oxc_ast::ast::ReturnStatement<'a>) {
            self.found = true;
        }
        fn visit_function(
            &mut self,
            _: &oxc_ast::ast::Function<'a>,
            _: oxc_syntax::scope::ScopeFlags,
        ) {
        }
        fn visit_arrow_function_expression(
            &mut self,
            _: &oxc_ast::ast::ArrowFunctionExpression<'a>,
        ) {
        }
    }
    let mut finder = ReturnFinder::default();
    for stmt in stmts {
        finder.visit_statement(stmt);
    }
    finder.found
}

#[allow(clippy::too_many_arguments)]
fn compile_eval_parts(
    program: ProgramParts<'_, '_>,
    kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
    forbid_var_arguments: bool,
    caller_chain: Option<&EvalCallerChain>,
    new_target_allowed: bool,
    in_class_field_initializer: bool,
    super_property_allowed: bool,
    super_call_allowed: bool,
    function_constructor: bool,
) -> Result<BytecodeModule, CompileError> {
    if !super_call_allowed && super_property_allowed && statements_contain_super_call(program.body)
    {
        return Err(CompileError::Unsupported {
            node: "SyntaxError: super() call is not allowed in this eval code".to_string(),
            span: program.span,
        });
    }
    {
        // §19.2.1.1 step 5 — `new.target` in eval code is an early
        // SyntaxError unless the direct-eval call site sits inside
        // non-arrow function code (arrows are transparent).
        // §19.2.1.1 / §15.7.1 — eval code in a class field
        // initializer may not reference `arguments` (functions and
        // static blocks inside the body open their own scope).
        if in_class_field_initializer
            && crate::strict_validation::program_contains_arguments(program.body)
        {
            return Err(CompileError::Unsupported {
                node:
                    "SyntaxError: 'arguments' is not allowed in class field initializer eval code"
                        .to_string(),
                span: program.span,
            });
        }
        if !new_target_allowed && capture::program_references_new_target(program.body) {
            return Err(CompileError::Unsupported {
                node: "SyntaxError: new.target expression is not allowed here".to_string(),
                span: program.span,
            });
        }
        if forbid_var_arguments && !(force_strict || program.strict_directive) {
            let mut var_names: Vec<String> = Vec::new();
            hoist_var_names(program.body, &mut var_names);
            if var_names.iter().any(|name| name == "arguments") {
                return Err(CompileError::Unsupported {
                    node: "SyntaxError: eval body may not var-declare 'arguments' here".to_string(),
                    span: program.span,
                });
            }
        }
        compile_program_for_eval(
            program,
            kind,
            module_specifier,
            force_strict,
            caller_chain,
            new_target_allowed,
            super_property_allowed,
            super_call_allowed,
            function_constructor,
        )
    }
}

/// Compile source text into the frozen runtime boundary product.
///
/// This is the preferred compiler/runtime contract for loaded script sources.
///
/// # Errors
/// Returns [`CompileError`] when parsing or lowering fails.
pub fn compile_script_source_to_module(
    source: &str,
    kind: SyntaxSourceKind,
    module_specifier: &str,
) -> Result<CompiledModule, CompileError> {
    let bytecode = compile_script_source(source, kind, module_specifier)?;
    CompiledModule::from_bytecode(bytecode)
}

/// Compile an already parsed OXC program as a script.
///
/// This keeps callers that need a syntax pass for routing or analysis from
/// parsing the same source twice. The caller must pass the same source kind
/// that was used to create `program`.
///
/// # Errors
/// Returns [`CompileError`] when the AST contains constructs outside the
/// foundation subset.
pub fn compile_script_program(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
) -> Result<BytecodeModule, CompileError> {
    compile_program(program, source_kind, module_specifier, false)
}

pub(crate) fn compile_program(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
) -> Result<BytecodeModule, CompileError> {
    compile_program_with_mode(program, source_kind, module_specifier, force_strict)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_program_for_eval(
    program: ProgramParts<'_, '_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
    caller_chain: Option<&EvalCallerChain>,
    new_target_allowed: bool,
    super_property_allowed: bool,
    super_call_allowed: bool,
    function_constructor: bool,
) -> Result<BytecodeModule, CompileError> {
    compile_program_parts(
        program,
        source_kind,
        module_specifier,
        ProgramMode {
            force_strict,
            eval_mode: true,
            caller_chain,
            new_target_allowed,
            super_property_allowed,
            super_call_allowed,
            function_constructor,
        },
    )
}

pub(crate) fn compile_program_with_mode(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
) -> Result<BytecodeModule, CompileError> {
    compile_program_parts(
        ProgramParts::of(program),
        source_kind,
        module_specifier,
        ProgramMode {
            force_strict,
            eval_mode: false,
            caller_chain: None,
            new_target_allowed: false,
            super_property_allowed: false,
            super_call_allowed: false,
            function_constructor: false,
        },
    )
}

/// Borrowed program surface: real `Program`s and eval bodies
/// re-parsed through the super-property method wrapper both lower
/// through the same pipeline.
pub(crate) struct ProgramParts<'a, 'b> {
    pub(crate) body: &'b [oxc_ast::ast::Statement<'a>],
    pub(crate) directives: &'b [oxc_ast::ast::Directive<'a>],
    pub(crate) span: (u32, u32),
    pub(crate) strict_directive: bool,
    /// Full source text the program was parsed from. Function spans
    /// are byte offsets into this string, so it backs the §20.2.3.5
    /// [[SourceText]] slice attached to each compiled function.
    pub(crate) source_text: &'a str,
}

impl<'a, 'b> ProgramParts<'a, 'b> {
    pub(crate) fn of(program: &'b Program<'a>) -> Self {
        Self {
            body: &program.body,
            directives: &program.directives,
            span: (program.span.start, program.span.end),
            strict_directive: program.has_use_strict_directive(),
            source_text: program.source_text,
        }
    }
}

/// Seal per-function facts derived from the finished bodies: the
/// `[[SourceText]]` ranges and whether each body ignores its `this`.
fn finish_bytecode(module: &mut BytecodeModule, source: &str) {
    attach_source_text(module, source);
    let observed: Vec<bool> = module
        .functions
        .iter()
        .map(|function| function.body_observes_this(&module.constants, &module.functions, 0))
        .collect();
    for (function, observed) in module.functions.iter_mut().zip(observed) {
        function.ignores_this = !observed;
    }
}

/// Attach §20.2.3.5 [[SourceText]] to every compiled function as a
/// validated range into one shared module-level source snapshot. The
/// `<main>` script/eval/module entry (function 0) is skipped — it is
/// never observable through `Function.prototype.toString`. Spans that
/// fall outside the source or on a non-char boundary (synthesized
/// functions) leave the range `None`, so `toString` keeps the
/// `NativeFunction` form for them. The snapshot is retained once per
/// module instead of one owned slice per function.
fn attach_source_text(module: &mut BytecodeModule, source: &str) {
    let mut any_range = false;
    for function in module.functions.iter_mut().skip(1) {
        // A method / accessor reports its `MethodDefinition` source,
        // whose range is wider than the function body `span` (it
        // includes the key and any `get`/`set`/`*`/`async` prefix).
        // Without that explicit range the bare body span would yield a
        // non-parseable fragment, so leave such functions in the
        // `NativeFunction` form until their definition span is recorded.
        let range = match function.source_text_span {
            Some(range) => range,
            None if function.is_method => continue,
            None => function.span,
        };
        let (start, end) = range;
        if start >= end {
            continue;
        }
        if source.get(start as usize..end as usize).is_some() {
            function.source_text_range = Some((start, end));
            any_range = true;
        }
    }
    if any_range {
        module.function_source = Some(source.to_string());
    }
}

/// How a script or eval body lowers.
pub(crate) struct ProgramMode<'c> {
    pub(crate) force_strict: bool,
    pub(crate) eval_mode: bool,
    /// Direct-eval caller context chain; `None` for scripts, indirect
    /// eval, and `Function` constructor bodies.
    pub(crate) caller_chain: Option<&'c EvalCallerChain>,
    pub(crate) new_target_allowed: bool,
    pub(crate) super_property_allowed: bool,
    pub(crate) super_call_allowed: bool,
    pub(crate) function_constructor: bool,
}

/// `true` when `cx` lowers the top level of a sloppy direct eval whose
/// variable environment is a caller function's (§19.2.1.3 step 16): its
/// `var` and function declarations bind in that scope, not locally.
fn in_function_var_env_eval(cx: &Compiler) -> bool {
    cx.stack.len() == 1
        && cx.scopes.len() == 1
        && cx.in_eval
        && !cx.is_strict
        && cx
            .eval_chain
            .as_ref()
            .is_some_and(|chain| chain.var_depth.is_some())
}

/// Variable-scope target of a sloppy direct eval's top-level function
/// declaration named `name`: the caller's static var-scope slot, or its
/// eval extension. `None` outside such an eval body.
pub(crate) fn eval_var_function_target(
    cx: &Compiler,
    name: &str,
) -> Option<crate::compiler::VarTarget> {
    if !in_function_var_env_eval(cx) {
        return None;
    }
    let chain = cx.eval_chain.as_ref()?;
    let var_depth = chain.var_depth?;
    let depth = u16::try_from(var_depth).ok()?;
    let scope = chain.scopes.get(var_depth as usize)?;
    match scope
        .descriptor
        .slots
        .iter()
        .position(|slot| slot.name == name)
    {
        Some(slot) => Some(crate::compiler::VarTarget::Outer {
            depth,
            slot: slot as u16,
        }),
        None => Some(crate::compiler::VarTarget::Extension { depth }),
    }
}

/// [`eval_var_function_target`] for a declaration instantiated at the eval
/// prologue: an extension target gets its deletable binding now
/// (`DeclareEvalVar`, §19.2.1.3 step 16 / §B.3.2.3).
pub(crate) fn eval_var_declaration_target(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Option<crate::compiler::VarTarget> {
    let target = eval_var_function_target(cx, name)?;
    if let crate::compiler::VarTarget::Extension { depth } = target {
        let name_idx = cx.intern_string_constant(name);
        cx.emit_ctx(
            Op::DeclareEvalVar,
            vec![
                Operand::Register(0),
                Operand::ConstIndex(name_idx),
                Operand::Imm32(i32::from(depth)),
            ],
            0,
            crate::scope::CtxReg::Closure,
            span,
        );
    }
    Some(target)
}

/// §19.2.1.3 step 3 — a sloppy direct eval may not hoist a `var` over a
/// lexical declaration between its call site and its variable scope
/// (Annex B.3.4 exempts simple catch parameters). The caller chain's
/// descriptors carry every binding such an eval can see.
fn check_eval_var_conflicts(
    chain: &EvalCallerChain,
    var_names: &[String],
    span: (u32, u32),
) -> Result<(), CompileError> {
    use otter_bytecode::{ScopeKind, SlotKind};
    let last = match chain.var_depth {
        Some(depth) => depth as usize,
        None => chain.scopes.len().saturating_sub(1),
    };
    for (hop, scope) in chain.scopes.iter().enumerate().take(last + 1) {
        if scope.descriptor.kind == ScopeKind::With {
            continue;
        }
        let is_var_scope = chain.var_depth == Some(hop as u32);
        for name in var_names {
            let Some(slot) = scope
                .descriptor
                .slots
                .iter()
                .find(|slot| &slot.name == name)
            else {
                continue;
            };
            let conflicts = if is_var_scope {
                matches!(slot.kind, SlotKind::Let | SlotKind::Const | SlotKind::Class)
            } else {
                !matches!(slot.kind, SlotKind::CatchParam { simple: true })
            };
            if conflicts {
                return Err(CompileError::Unsupported {
                    node: format!("SyntaxError: Identifier '{name}' has already been declared"),
                    span,
                });
            }
        }
    }
    Ok(())
}

/// `true` when an Annex B block function named `name` is shadowed by a
/// binding between a sloppy direct eval and its variable scope, which
/// skips its var extension (§B.3.2.3 bindingExists).
pub(crate) fn eval_annex_b_blocked(cx: &Compiler, name: &str) -> bool {
    let Some(chain) = cx.eval_chain.as_ref() else {
        return false;
    };
    let last = match chain.var_depth {
        Some(depth) => depth as usize,
        None => chain.scopes.len(),
    };
    chain.scopes.iter().take(last).any(|scope| {
        scope.descriptor.kind != otter_bytecode::ScopeKind::With
            && scope.descriptor.slots.iter().any(|slot| slot.name == name)
    })
}

/// Rebuild the compile-time state a direct eval inherits from its caller
/// chain: the `with` object environments (§9.1.1.2.1) and the
/// PrivateEnvironment (§19.2.1.1) of enclosing classes.
fn inherit_eval_chain_state(cx: &mut Compiler) {
    use otter_bytecode::{ScopeKind, SlotKind};
    let Some(chain) = cx.eval_chain.clone() else {
        return;
    };
    // Outermost first, so the innermost entry ends up last.
    for hop in (0..chain.scopes.len()).rev() {
        let descriptor = &chain.scopes[hop].descriptor;
        let location = crate::compiler::ScopeLocation::Chain { hop };
        match descriptor.kind {
            ScopeKind::With => cx
                .active_with_envs
                .push(crate::with_statement::WithEnv { location }),
            ScopeKind::Class => {
                let names: std::collections::HashSet<String> = descriptor
                    .slots
                    .iter()
                    .filter(|slot| slot.kind == SlotKind::PrivateName)
                    .filter_map(|slot| slot.name.strip_prefix('#').map(str::to_string))
                    .collect();
                let namespace = {
                    let module = Rc::clone(&cx.top_mut().module);
                    let mut m = module.borrow_mut();
                    let id = m.next_private_namespace;
                    m.next_private_namespace = id.saturating_add(1);
                    id
                };
                cx.private_namespaces.push(namespace);
                cx.class_private_names.push(names);
                cx.class_private_instance_methods
                    .push(std::collections::HashSet::new());
                cx.class_scope_locations.push(location);
            }
            _ => {}
        }
    }
}

/// Lower a script or eval body into a stand-alone module whose function 0
/// is `<main>`.
pub(crate) fn compile_program_parts(
    program: ProgramParts<'_, '_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
    mode: ProgramMode<'_>,
) -> Result<BytecodeModule, CompileError> {
    let ProgramMode {
        force_strict,
        eval_mode,
        caller_chain,
        new_target_allowed,
        super_property_allowed,
        super_call_allowed,
        function_constructor,
    } = mode;
    let source_text = program.source_text;
    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    let script_module_url = if module_specifier.starts_with("file://") {
        module_specifier.to_string()
    } else {
        Default::default()
    };
    // §12.9.3.1 + §15.7 strict-mode early errors that oxc_parser
    // does not flag on its own.
    strict_validation::validate_strict_mode_early_errors(
        program.directives,
        program.body,
        force_strict || program.strict_directive,
        super_property_allowed || super_call_allowed,
    )?;
    if !new_target_allowed {
        strict_validation::validate_script_new_target_early_errors(program.body)?;
    }
    // §14.2.1 / §14.12.1 block-level lexical early errors.
    strict_validation::validate_block_early_errors(
        program.body,
        force_strict || program.strict_directive,
    )?;
    let main_is_async = module_body_uses_top_level_await(program.body);
    let main_is_strict = force_strict || program.strict_directive;
    let caller_chain = if eval_mode { caller_chain } else { None };
    let var_depth = caller_chain.and_then(|chain| chain.var_depth);
    // §19.2.1.1 — a strict eval owns its variable environment; a sloppy
    // direct eval in a function extends the caller's; everything else
    // (scripts, sloppy global-level evals) declares on the global object.
    let function_var_env = eval_mode && !main_is_strict && var_depth.is_some();
    let global_var_bindings = !eval_mode || (!main_is_strict && var_depth.is_none());
    module.borrow_mut().functions.push(Function {
        id: 0,
        name: "<main>".to_string(),
        span: program.span,
        is_async: main_is_async,
        is_strict: main_is_strict,
        module_url: script_module_url.clone(),
        ..Default::default()
    });
    let mut top = FunctionContext::new(Rc::clone(&module))
        .with_strict(main_is_strict)
        .with_module_url(script_module_url);
    // A nested closure or direct eval reaching a body binding keeps it in
    // a slot (every body binding when the body contains a direct eval).
    let unit_capture = capture::CaptureFacts::of_unit(program.body);
    top.captured_names = unit_capture.captured(unit_capture.unit(), false);
    top.dot_arguments_observed = unit_capture.reads_dot_arguments();
    top.contains_direct_eval = unit_capture.unit_contains_eval();

    let mut top_level_vars: Vec<String> = Vec::new();
    hoist_var_names(program.body, &mut top_level_vars);
    // §19.2.1.3 step 3 — var/lexical conflicts against the caller chain.
    if eval_mode
        && !main_is_strict
        && let Some(chain) = caller_chain
    {
        check_eval_var_conflicts(chain, &top_level_vars, program.span)?;
    }

    // §19.2.1.1 — a nested direct eval inside this eval body inherits
    // the caller's `super` legality.
    top.has_home_object = super_property_allowed;
    top.is_derived_ctor = super_call_allowed;
    // `this` of a `super()`-capable eval is the caller constructor's
    // `DerivedThis` slot when it has one.
    top.eval_this_from_chain = super_call_allowed
        && caller_chain.is_some_and(|chain| {
            chain.scopes.iter().any(|scope| {
                scope
                    .descriptor
                    .slots
                    .iter()
                    .any(|slot| slot.kind == otter_bytecode::SlotKind::DerivedThis)
            })
        });
    // A script `<main>` and an indirect eval run with no closure context.
    top.closure_context_empty = caller_chain.is_none();
    let mut cx = Compiler::new(top, unit_capture);
    cx.eval_chain = caller_chain.cloned();
    cx.suppress_global_mirror = eval_mode && (main_is_strict || function_var_env);
    cx.in_eval = eval_mode;
    cx.eval_new_target_allowed = new_target_allowed;
    // §20.2.1.1 step 32 — the dynamic function's outer NFE gets no
    // self-name binding: `anonymous` inside the body resolves free.
    if function_constructor {
        cx.next_fn_expr_no_self_binding = true;
    }
    let scope_kind = if !eval_mode {
        otter_bytecode::ScopeKind::Block
    } else if main_is_strict {
        otter_bytecode::ScopeKind::EvalVar
    } else {
        otter_bytecode::ScopeKind::EvalLexical
    };
    cx.enter_scope_with_flags(
        scope_kind,
        otter_bytecode::ScopeFlags {
            strict: main_is_strict,
            var_scope: eval_mode && main_is_strict,
            has_extension: false,
        },
    );
    inherit_eval_chain_state(&mut cx);

    // §16.1.7 GlobalDeclarationInstantiation step 16 / §19.2.1.3
    // EvalDeclarationInstantiation step 16.a — script global code and
    // sloppy global-level eval code create their top-level `var` /
    // function bindings on the global object's environment record.
    // Script bindings are non-configurable; eval bindings are deletable.
    let program_span = program.span;
    if global_var_bindings {
        cx.script_global_vars = top_level_vars.iter().cloned().collect();
        // §16.1.7 steps 1–12 / §19.2.1.3 steps 5–11 — validate every
        // declared name before any binding is created so a failing
        // script instantiates nothing: lexicals first, then function
        // declarations, then plain vars.
        let function_names: HashSet<String> = top_level_hoistable_function_names(program.body)
            .into_iter()
            .collect();
        let mut validate_lex: Vec<(String, bool)> = Vec::new();
        if !eval_mode {
            hoist_lexical_names(program.body, &mut validate_lex);
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let mut validations: Vec<(&str, i32)> = Vec::new();
        for (name, _) in &validate_lex {
            if seen.insert(name.as_str()) {
                validations.push((name.as_str(), 0));
            }
        }
        for name in &top_level_vars {
            if seen.insert(name.as_str()) {
                let kind = if function_names.contains(name.as_str()) {
                    2
                } else {
                    1
                };
                validations.push((name.as_str(), kind));
            }
        }
        for (name, kind) in validations {
            let name_idx = cx.intern_string_constant(name);
            cx.emit(
                Op::ValidateGlobalDecl,
                [Operand::ConstIndex(name_idx), Operand::Imm32(kind)],
                program_span,
            );
        }
    } else if function_var_env {
        // §19.2.1.3 steps 16–17 — a name the caller's variable scope does
        // not bind statically becomes a deletable binding of its eval
        // extension; a statically bound one is re-bound in place.
        let mut seen: HashSet<&str> = HashSet::new();
        for name in &top_level_vars {
            if seen.insert(name.as_str()) {
                eval_var_declaration_target(&mut cx, name, program_span);
            }
        }
    } else {
        pre_declare_var_bindings(&mut cx, &top_level_vars, program_span)?;
    }
    // §B.3.3.2/3 — sloppy script / eval bodies extend the variable
    // scope with block-level function declaration names.
    pre_declare_annex_b_functions(
        &mut cx,
        program.body,
        &std::collections::HashSet::new(),
        program_span,
    )?;
    // §10.2.11 step 33 — pre-declare top-level `let` / `const` /
    // `class` names with TDZ. Script global code instead declares them
    // on the realm's global declarative record (§16.1.7 step 15); eval
    // lexicals stay private to the eval body (§19.2.1.1).
    let mut top_level_lex: Vec<(String, bool)> = Vec::new();
    hoist_lexical_names(program.body, &mut top_level_lex);
    if !eval_mode {
        cx.script_global_lexicals = top_level_lex.iter().map(|(name, _)| name.clone()).collect();
        let mut declared: HashSet<&str> = HashSet::new();
        for (name, is_const) in &top_level_lex {
            if !declared.insert(name.as_str()) {
                continue;
            }
            let name_idx = cx.intern_string_constant(name);
            cx.emit(
                Op::DeclareGlobalLex,
                [
                    Operand::ConstIndex(name_idx),
                    Operand::Imm32(i32::from(*is_const)),
                ],
                program_span,
            );
        }
    } else {
        pre_declare_lexical_bindings(&mut cx, &top_level_lex, program_span)?;
    }
    // §10.2.11 step 30 — top-level function declarations hoist so calls
    // before the source-level declaration resolve to the function value.
    hoist_function_declarations(&mut cx, program.body)?;
    if global_var_bindings {
        let function_names: HashSet<String> = top_level_hoistable_function_names(program.body)
            .into_iter()
            .collect();
        let mut declared: HashSet<&str> = HashSet::new();
        for name in &top_level_vars {
            if !declared.insert(name.as_str()) || function_names.contains(name.as_str()) {
                continue;
            }
            // §9.1.1.4.17 CreateGlobalVarBinding(name, configurable).
            let name_idx = cx.intern_string_constant(name);
            cx.emit(
                Op::DeclareGlobalVar,
                [
                    Operand::ConstIndex(name_idx),
                    Operand::Imm32(i32::from(eval_mode)),
                ],
                program_span,
            );
        }
    }

    // §8.4 — the program completion register (spec `V`).
    let completion_reg = cx.alloc_scratch();
    cx.emit(
        Op::LoadUndefined,
        [Operand::Register(completion_reg)],
        program_span,
    );
    cx.completion_reg = Some(completion_reg);
    // A directive prologue contributes its string values to the script /
    // `eval` completion value (so `eval('"x"')` evaluates to `"x"`).
    for directive in program.directives {
        let dst = cx.alloc_scratch();
        let const_idx = cx.intern_string_constant(&directive.expression.value);
        cx.emit(
            Op::LoadString,
            [Operand::Register(dst), Operand::ConstIndex(const_idx)],
            (directive.span.start, directive.span.end),
        );
        cx.emit_completion_value(dst, (directive.span.start, directive.span.end));
    }
    for stmt in program.body {
        compile_discarded_statement(&mut cx, stmt)?;
    }
    cx.exit_scope();

    // The program completion value is whatever the completion
    // register holds when the body finishes.
    let span = program.span;
    cx.emit(Op::Return, [Operand::Register(completion_reg)], span);

    {
        let finished = cx.finish_code(span);
        if cx.register_overflow {
            return Err(CompileError::Unsupported {
                node: "program body exhausts the 65535-register window".to_string(),
                span,
            });
        }
        let contains_direct_eval = cx.contains_direct_eval;
        cx.take_class_hint_sites(0, finished.class_hint_sites);
        let mut m = module.borrow_mut();
        m.functions[0].locals = 0;
        m.functions[0].scratch = finished.scratch;
        m.functions[0].scopes = finished.scopes;
        m.functions[0].contains_direct_eval = contains_direct_eval;
        m.functions[0].number_hint_sites = finished.number_hint_sites;
        crate::type_hints::resolve_class_hint_sites(&cx, &mut m.functions);
        m.functions[0].code = finished.code;
        m.functions[0].handlers = finished.handlers;
        m.functions[0].spans = otter_bytecode::SpanTable::new(&finished.spans);
    }
    drop(cx);

    let kind = bytecode_source_kind(source_kind);

    let ModuleBuilder {
        functions,
        constants,
        template_sites,
        next_private_namespace: _,
        ..
    } = Rc::try_unwrap(module)
        .expect("module builder should be uniquely owned at finalize")
        .into_inner();

    let mut bytecode = BytecodeModule {
        module: module_specifier.to_string(),
        template_sites,
        source_kind: kind,
        functions,
        constants,
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
        function_source: None,
    };
    finish_bytecode(&mut bytecode, source_text);
    Ok(bytecode)
}

/// Compile a parsed program as one ES-module fragment.
///
/// The output is a stand-alone [`BytecodeModule`] with a single
/// `<module-init>` function (id 0) carrying `is_module = true` +
/// `module_url` set, plus the `module_resolutions` table populated
/// from `host.resolved_imports`. The runtime's module-graph driver
/// chains these fragments through the linker into a unified
/// `BytecodeModule`.
///
/// # Algorithm (spec mapping: ECMA-262 §16.2 Modules)
/// 1. Run an import pre-pass over the program body, numbering one
///    import record per source specifier and recording each
///    importer-side alias → `(record, source_name)`
///    binding (§16.2.2 ModuleNamespaceObject for `import * as`,
///    §16.2.3 ImportEntry for named imports).
/// 2. Run an export pre-pass to collect the names this module
///    exports (§16.2.3 ExportEntry). Every later assignment to one
///    of those names emits an extra `StoreProperty module_env,
///    name, value` so live bindings propagate across modules.
/// 3. Declare module-scope slots for `module_env` (param 0) and
///    `import_meta` (param 1) and store the parameters into them at
///    entry, so closures defined inside the body reach them through
///    their context chain. The module scope's context is allocated by
///    the runtime from descriptor 0 and shared by the link and
///    evaluation invocations.
/// 4. Declare one module-scope slot per import source and fill it with
///    `Op::ImportNamespace`.
/// 5. Compile the rest of the body via the existing
///    [`compile_statement`] path; the import / export awareness
///    stays in [`FunctionContext::module_state`] and the identifier
///    resolution paths consult it.
/// 6. Emit `Op::ReturnUndefined` at the tail.
///
/// # Errors
/// - [`CompileError::Syntax`] on parse-level failures.
/// - [`CompileError::Unsupported`] for foundation-out-of-scope
///   constructs.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-modules>
/// - <https://tc39.es/ecma262/#sec-source-text-module-records>
///
/// Compile an already parsed OXC program as one ES-module fragment.
///
/// This is the module-graph hot path: callers that already borrowed an AST for
/// import collection can lower the same AST without reparsing.
///
/// # Errors
/// - [`CompileError::Unsupported`] for foundation-out-of-scope constructs.
/// - [`CompileError::TypeScriptUnsupported`] for unsupported TS syntax that
///   survives parser erasure.
pub fn compile_module_program(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    host: &ModuleHostInfo,
) -> Result<BytecodeModule, CompileError> {
    // §12.9.3.1 + §15.7 strict-mode early errors. Module bodies are
    // always strict mode code (§10.2.10).
    strict_validation::validate_strict_mode_early_errors(
        &program.directives,
        &program.body,
        true,
        false,
    )?;
    // §14.2.1 / §14.12.1 block-level lexical early errors; module
    // code is always strict so no Annex B exemption applies.
    strict_validation::validate_block_early_errors(&program.body, true)?;
    strict_validation::validate_module_early_errors(program)?;
    // §16.2.1 Static Semantics: Early Errors — `ImportDeclaration`
    // and `ExportDeclaration` are `ModuleItem` productions, not
    // statements; they may appear only directly under `ModuleBody`.
    // Nested occurrences (inside a `Block`, `IfStatement`, loop
    // body, etc.) are early `SyntaxError`s.
    // <https://tc39.es/ecma262/#sec-module-semantics-static-semantics-early-errors>
    validate_module_item_positions(program)?;
    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    let init_is_async = module_body_uses_top_level_await(&program.body);
    module.borrow_mut().functions.push(Function {
        id: 0,
        name: "<module-init>".to_string(),
        span: (program.span.start, program.span.end),
        is_module: true,
        is_async: init_is_async,
        is_strict: true,
        module_url: host.module_url.clone(),
        // module_env, import_meta, link-phase flag (§16.2.1.7 — a
        // truthy third argument runs only the InitializeEnvironment
        // prologue and returns before the body).
        param_count: 3,
        ..Default::default()
    });

    let mut top = FunctionContext::new(Rc::clone(&module))
        .with_strict(true)
        .with_module_url(host.module_url.clone());
    let unit_capture = capture::CaptureFacts::of_unit(&program.body);
    top.captured_names = unit_capture.captured(unit_capture.unit(), false);
    // Hoisted function declarations instantiate during the link-phase
    // init invocation; the evaluation-phase invocation (a separate
    // frame over the same module context) must observe the same closure
    // values, so their bindings live in module-scope slots.
    for name in top_level_hoistable_function_names(&program.body) {
        top.captured_names.insert(name);
    }
    top.contains_direct_eval = unit_capture.unit_contains_eval();

    let mut state = ModuleState {
        pre_resolved_imports: host.resolved_imports.clone(),
        ..ModuleState::default()
    };
    let mut next_record: u16 = 0;
    let mut allocate_record = || {
        let record = next_record;
        next_record = next_record.checked_add(1).expect("import record overflow");
        record
    };

    // Pre-pass: collect import sources + number their records; collect
    // exported names + import bindings.
    let mut import_sources_in_order: Vec<ImportRequest> = Vec::new();
    let mut deferred_sources_in_order: Vec<ImportRequest> = Vec::new();
    // §16.2.1.7 InitializeEnvironment — exported binding slots that
    // must exist on the module environment from instantiation so the
    // namespace reports them (`'x' in ns`) and an access before the
    // declaration runs is a TDZ `ReferenceError`. `(exported_name,
    // local_name, is_var)`: `var` slots initialize to `undefined`,
    // lexical / function / class slots to the TDZ hole. Re-export
    // (`export … from`) names are resolved elsewhere and excluded.
    let mut tdz_inline: Vec<(String, bool)> = Vec::new();
    let mut local_export_specs: Vec<(String, String)> = Vec::new();
    // Statically-known re-export names (`export { x } from m`,
    // `export * as ns from m`) — pre-declared as TDZ holes so the
    // namespace reports them and an access before the re-export
    // statement copies the value is a ReferenceError. (Bare `export *`
    // names are not statically known and are filled by StarReexport.)
    let mut reexport_tdz: Vec<String> = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(decl) if !decl.import_kind.is_type() => {
                // The `type` attribute is half the request key: the same
                // specifier read as two formats is two targets and two
                // records, never one shared binding.
                let request = ImportRequest::new(
                    decl.source.value.as_str(),
                    import_attribute_type(decl.with_clause.as_deref()),
                );
                // TC39 import defer — `import defer * as ns from "x"`
                // defers evaluation until the namespace is accessed.
                // The grammar permits the namespace form only; named,
                // default, and bare `import defer "x"` are early
                // SyntaxErrors.
                let is_defer_phase = matches!(decl.phase, Some(oxc_ast::ast::ImportPhase::Defer));
                if is_defer_phase {
                    let is_namespace_only = decl
                        .specifiers
                        .as_ref()
                        .map(|specs| {
                            specs.len() == 1
                                && matches!(
                                    specs[0],
                                    oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(_)
                                )
                        })
                        .unwrap_or(false);
                    if !is_namespace_only {
                        return Err(CompileError::Syntax {
                            messages: vec![
                                "SyntaxError: `import defer` may only be used with a namespace import (`import defer * as ns from \"...\"`)"
                                    .to_string(),
                            ],
                            diagnostics: Vec::new(),
                        });
                    }
                }
                // Deferred imports bind to a dedicated record so they are
                // not pulled into the eager-evaluation set and stay
                // distinct from any eager namespace of the same module.
                let record = if is_defer_phase {
                    if let Some(&record) = state.deferred_import_records.get(&request) {
                        record
                    } else {
                        let record = allocate_record();
                        state
                            .deferred_import_records
                            .insert(request.clone(), record);
                        deferred_sources_in_order.push(request.clone());
                        record
                    }
                } else {
                    if !state.import_records.contains_key(&request) {
                        let record = allocate_record();
                        state.import_records.insert(request.clone(), record);
                        import_sources_in_order.push(request.clone());
                    }
                    state.import_records[&request]
                };
                if let Some(specifiers) = &decl.specifiers {
                    for spec in specifiers.iter() {
                        match spec {
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                let alias = s.local.name.as_str().to_string();
                                let source_name = match &s.imported {
                                    oxc_ast::ast::ModuleExportName::IdentifierName(id) => {
                                        id.name.as_str().to_string()
                                    }
                                    oxc_ast::ast::ModuleExportName::IdentifierReference(id) => {
                                        id.name.as_str().to_string()
                                    }
                                    oxc_ast::ast::ModuleExportName::StringLiteral(lit) => {
                                        lit.value.as_str().to_string()
                                    }
                                };
                                state.imported_names.insert(
                                    alias,
                                    ImportBinding {
                                        record,
                                        source_name,
                                        is_namespace: false,
                                        request: request.clone(),
                                        is_deferred: is_defer_phase,
                                    },
                                );
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                let alias = s.local.name.as_str().to_string();
                                state.imported_names.insert(
                                    alias,
                                    ImportBinding {
                                        record,
                                        source_name: "default".to_string(),
                                        is_namespace: false,
                                        request: request.clone(),
                                        is_deferred: is_defer_phase,
                                    },
                                );
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(
                                s,
                            ) => {
                                let alias = s.local.name.as_str().to_string();
                                state.imported_names.insert(
                                    alias,
                                    ImportBinding {
                                        record,
                                        source_name: String::new(),
                                        is_namespace: true,
                                        request: request.clone(),
                                        is_deferred: is_defer_phase,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
                if let Some(inner) = &decl.declaration {
                    match inner {
                        oxc_ast::ast::Declaration::VariableDeclaration(var_decl) => {
                            let is_var =
                                matches!(var_decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
                            // §16.2.3.2 ExportedBindings — BoundNames of the
                            // declaration, including every leaf of a
                            // destructuring pattern (`export const { check }
                            // = …` exports `check`).
                            for declarator in &var_decl.declarations {
                                let mut names = Vec::new();
                                collect_pattern_var_names(&declarator.id, &mut names);
                                for name in names {
                                    state.exported_names.insert(name.clone());
                                    tdz_inline.push((name, is_var));
                                }
                            }
                        }
                        oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
                            if let Some(id) = &f.id {
                                let name = id.name.as_str().to_string();
                                state.exported_names.insert(name.clone());
                                tdz_inline.push((name, false));
                            }
                        }
                        oxc_ast::ast::Declaration::ClassDeclaration(c) => {
                            if let Some(id) = &c.id {
                                let name = id.name.as_str().to_string();
                                state.exported_names.insert(name.clone());
                                tdz_inline.push((name, false));
                            }
                        }
                        _ => {}
                    }
                }
                // §16.2.3 ExportFromClause — `export {x} from "./other"`
                // also references another module. Register the source
                // in `import_records` so the body-compile arm can
                // look it up via `state.import_records.get(src)`.
                // Without this the body raises
                // `ExportNamedDeclaration: unresolved re-export
                // source` even though the AST is well-formed and the
                // module loader has the target module available.
                // <https://tc39.es/ecma262/#sec-exports>
                if let Some(source) = decl.source.as_ref() {
                    let request = ImportRequest::new(
                        source.value.as_str(),
                        import_attribute_type(decl.with_clause.as_deref()),
                    );
                    if !state.import_records.contains_key(&request) {
                        let record = allocate_record();
                        state.import_records.insert(request.clone(), record);
                        import_sources_in_order.push(request);
                    }
                }
                // A re-export whose source resolves to this very module
                // (`export { x } from "./self"`) is an indirect binding
                // to our own local `x` — treat it like a local re-export
                // so it tracks later writes (live binding) rather than
                // snapshotting at the export statement.
                let self_source = decl
                    .source
                    .as_ref()
                    .map(|s| {
                        ImportRequest::new(
                            s.value.as_str(),
                            import_attribute_type(decl.with_clause.as_deref()),
                        )
                    })
                    .and_then(|request| host.resolved_imports.get(&request))
                    .is_some_and(|target| *target == host.module_url);
                let has_source = decl.source.is_some();
                for spec in &decl.specifiers {
                    let exported_name = module_export_name_to_str(&spec.exported);
                    state.exported_names.insert(exported_name.clone());
                    // `export { local as exported }` (no `from`) mirrors a
                    // local binding onto the env; its slot must be
                    // pre-declared. Re-export specs (`export … from`) are
                    // resolved separately.
                    if has_source {
                        if self_source {
                            // imported name === our local binding name.
                            let local_name = module_export_name_to_str(&spec.local);
                            state
                                .reexport_local_targets
                                .entry(local_name)
                                .or_default()
                                .push(exported_name.clone());
                        }
                        reexport_tdz.push(exported_name);
                    } else {
                        let local_name = module_export_name_to_str(&spec.local);
                        if local_name != exported_name {
                            state
                                .reexport_local_targets
                                .entry(local_name.clone())
                                .or_default()
                                .push(exported_name.clone());
                        }
                        local_export_specs.push((exported_name, local_name));
                    }
                }
            }
            Statement::ExportAllDeclaration(decl) if !decl.export_kind.is_type() => {
                // §16.2.3 ExportFromClause — `export * from "./other"`
                // / `export * as ns from "./other"`. Register the
                // source so the body-compile arm can look it up.
                let request = ImportRequest::new(
                    decl.source.value.as_str(),
                    import_attribute_type(decl.with_clause.as_deref()),
                );
                if !state.import_records.contains_key(&request) {
                    let record = allocate_record();
                    state.import_records.insert(request.clone(), record);
                    import_sources_in_order.push(request);
                }
                if let Some(exported) = decl.exported.as_ref() {
                    let name = module_export_name_to_str(exported);
                    state.exported_names.insert(name.clone());
                    reexport_tdz.push(name);
                }
            }
            Statement::ExportDefaultDeclaration(decl) => {
                state.exported_names.insert("default".to_string());
                tdz_inline.push(("default".to_string(), false));
                // §16.2.3.7 — a *named* default function/class also
                // creates a module-scope binding; later writes to it
                // must mirror onto the `default` export slot (live
                // binding).
                let local_name = match &decl.declaration {
                    oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        f.id.as_ref().map(|id| id.name.as_str().to_string())
                    }
                    oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                        c.id.as_ref().map(|id| id.name.as_str().to_string())
                    }
                    _ => None,
                };
                if let Some(local_name) = local_name {
                    state
                        .reexport_local_targets
                        .entry(local_name)
                        .or_default()
                        .push("default".to_string());
                }
            }
            _ => {}
        }
    }

    let record_count = next_record;
    top.module_state = Some(state);

    let mut cx = Compiler::new(top, unit_capture);
    // The module scope's context (descriptor 0) is allocated by the runtime
    // once per module and handed to both `<module-init>` invocations (link
    // and evaluation) as their closure context.
    cx.enter_scope_with_flags(
        otter_bytecode::ScopeKind::Module,
        otter_bytecode::ScopeFlags {
            strict: true,
            var_scope: true,
            has_extension: false,
        },
    );
    cx.install_runtime_created_scope();
    let span0 = (program.span.start, program.span.end);
    // The module environment, `import.meta`, and every import record live
    // in module-scope slots, so nested functions reach them by name.
    let env_storage = cx.declare_forced_slot(
        crate::compiler::MODULE_ENV_BINDING,
        otter_bytecode::SlotKind::ModuleEnv,
        span0,
    )?;
    cx.mark_initialized(crate::compiler::MODULE_ENV_BINDING);
    let meta_storage = cx.declare_forced_slot(
        crate::compiler::IMPORT_META_BINDING,
        otter_bytecode::SlotKind::ImportMeta,
        span0,
    )?;
    cx.mark_initialized(crate::compiler::IMPORT_META_BINDING);
    let mut record_storages = Vec::with_capacity(usize::from(record_count));
    for record in 0..record_count {
        let name = crate::compiler::import_record_binding(record);
        let storage = cx.declare_forced_slot(&name, otter_bytecode::SlotKind::Synthetic, span0)?;
        cx.mark_initialized(&name);
        record_storages.push(storage);
    }

    // Params: r0 = module_env, r1 = import_meta, r2 = link-phase flag.
    cx.scratch = 3;
    cx.emit_store_storage(0, env_storage, span0);
    cx.emit_store_storage(1, meta_storage, span0);

    // For each import source, resolve its namespace into the record slot.
    for request in &import_sources_in_order {
        let record = cx.module_state.as_ref().unwrap().import_records[request];
        let scratch = cx.alloc_scratch();
        let target = import_target_constant(&cx, request);
        let target_const = cx.intern_string_constant(&target);
        cx.emit(
            Op::ImportNamespace,
            [
                Operand::Register(scratch),
                Operand::ConstIndex(target_const),
            ],
            span0,
        );
        cx.emit_store_storage(scratch, record_storages[usize::from(record)], span0);
    }

    // For each `import defer` source, resolve a deferred namespace object
    // without evaluating the module.
    for request in &deferred_sources_in_order {
        let record = cx.module_state.as_ref().unwrap().deferred_import_records[request];
        let scratch = cx.alloc_scratch();
        let target = import_target_constant(&cx, request);
        let target_const = cx.intern_string_constant(&target);
        cx.emit(
            Op::ImportNamespaceDeferred,
            [
                Operand::Register(scratch),
                Operand::ConstIndex(target_const),
            ],
            span0,
        );
        cx.emit_store_storage(scratch, record_storages[usize::from(record)], span0);
    }

    // §16.2.1.7 InitializeEnvironment — the prologue below runs only
    // during the link-phase invocation (third argument truthy); the
    // evaluation-phase invocation jumps straight to the body. Both
    // frames share the module's context, so the closures and TDZ slots
    // the link phase created stay visible.
    let eval_phase_jump = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(2), span0);

    // §16.2.1.7 InitializeEnvironment — pre-declare exported binding
    // slots on the module environment before hoisting, so the
    // namespace reports every export from instantiation and a read
    // before initialization is a TDZ `ReferenceError`. `var` slots
    // start `undefined`; lexical / function / class / default slots
    // start as the hole and are filled when their declaration runs
    // (function hoisting below overwrites its hole with the closure).
    {
        let mut var_name_set = std::collections::HashSet::new();
        let mut tmp = Vec::new();
        hoist_var_names(&program.body, &mut tmp);
        var_name_set.extend(tmp);
        let mut slots: Vec<(String, bool)> = tdz_inline.clone();
        for (exported, local) in &local_export_specs {
            slots.push((exported.clone(), var_name_set.contains(local)));
        }
        for exported in &reexport_tdz {
            slots.push((exported.clone(), false));
        }
        if !slots.is_empty() {
            let env_reg = crate::statements::module_env_register(&mut cx, span0)?;
            let mut seen = std::collections::HashSet::new();
            for (name, is_var) in slots {
                if !seen.insert(name.clone()) {
                    continue;
                }
                let val_reg = cx.alloc_scratch();
                cx.emit(
                    if is_var {
                        Op::LoadUndefined
                    } else {
                        Op::LoadHole
                    },
                    [Operand::Register(val_reg)],
                    span0,
                );
                cx.emit_store_property(env_reg, &name, val_reg, span0);
            }
        }
    }

    // §16.2.1.7 ModuleDeclarationInstantiation step 11 — hoist
    // every `var`-declared name in the module body to the
    // module-init function's variable scope, pre-bound to
    // `undefined`. The pass is identical to the script-level
    // `<main>` entry, just at the module-fragment level.
    let mut module_vars: Vec<String> = Vec::new();
    hoist_var_names(&program.body, &mut module_vars);
    pre_declare_var_bindings(&mut cx, &module_vars, span0)?;
    let mut module_lex: Vec<(String, bool)> = Vec::new();
    hoist_lexical_names(&program.body, &mut module_lex);
    pre_declare_lexical_bindings(&mut cx, &module_lex, span0)?;
    // §10.2.11 step 30 — top-level function declarations hoist to
    // the module scope so cross-references work regardless of
    // source order.
    hoist_function_declarations(&mut cx, &program.body)?;
    // Link phase ends here — the body belongs to evaluation.
    cx.emit(Op::ReturnUndefined, [], span0);
    cx.patch_branch_to_here(eval_phase_jump);

    for stmt in &program.body {
        compile_discarded_statement(&mut cx, stmt)?;
    }
    cx.exit_scope();

    cx.emit(Op::ReturnUndefined, [], span0);

    {
        let finished = cx.finish_code(span0);
        if cx.register_overflow {
            return Err(CompileError::Unsupported {
                node: "module body exhausts the 65535-register window".to_string(),
                span: span0,
            });
        }
        let contains_direct_eval = cx.contains_direct_eval;
        cx.take_class_hint_sites(0, finished.class_hint_sites);
        let mut m = module.borrow_mut();
        m.functions[0].locals = 0;
        m.functions[0].scratch = finished.scratch;
        m.functions[0].scopes = finished.scopes;
        m.functions[0].contains_direct_eval = contains_direct_eval;
        m.functions[0].number_hint_sites = finished.number_hint_sites;
        crate::type_hints::resolve_class_hint_sites(&cx, &mut m.functions);
        m.functions[0].code = finished.code;
        m.functions[0].handlers = finished.handlers;
        m.functions[0].spans = otter_bytecode::SpanTable::new(&finished.spans);
    }
    // Capture deferred import specifiers before dropping the compiler
    // so resolution edges can be flagged. A specifier imported both
    // eagerly and via `import defer` counts as eager for reachability
    // (the module evaluates eagerly regardless), so it is excluded.
    let deferred_only_specs: HashSet<ImportRequest> = {
        let ms = cx.module_state.as_ref();
        let eager: HashSet<&ImportRequest> = ms
            .map(|s| s.import_records.keys().collect())
            .unwrap_or_default();
        ms.map(|s| {
            s.deferred_import_records
                .keys()
                .filter(|k| !eager.contains(*k))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
    };
    drop(cx);

    let kind = bytecode_source_kind(source_kind);

    let ModuleBuilder {
        functions,
        constants,
        template_sites,
        next_private_namespace: _,
        ..
    } = Rc::try_unwrap(module)
        .expect("module builder should be uniquely owned at finalize")
        .into_inner();

    // Populate module_resolutions from host info: every specifier
    // → (referrer, specifier, target) triple. Edges whose specifier is
    // imported only via `import defer` are flagged so eager evaluation
    // skips them.
    let module_resolutions: Vec<otter_bytecode::ModuleResolution> = host
        .resolved_imports
        .iter()
        .map(|(request, target)| otter_bytecode::ModuleResolution {
            referrer: host.module_url.clone(),
            specifier: request.specifier.clone(),
            attr_type: request.attr_type.clone(),
            target: target.clone(),
            deferred: deferred_only_specs.contains(request),
            dynamic: false,
            synthetic: false,
        })
        .collect();

    let mut bytecode = BytecodeModule {
        module: host.module_url.clone(),
        template_sites,
        source_kind: kind,
        functions,
        constants,
        module_resolutions,
        module_inits: Vec::new(),
        function_source: None,
    };
    finish_bytecode(&mut bytecode, program.source_text);
    Ok(bytecode)
}

/// Names of top-level hoistable function declarations — plain
/// declarations, `export function`, and named `export default
/// function`. §16.2.1.7 InitializeEnvironment instantiates these
/// during the link phase.
fn top_level_hoistable_function_names(stmts: &[Statement<'_>]) -> Vec<String> {
    let mut out = Vec::new();
    for stmt in stmts {
        let f = match stmt {
            Statement::FunctionDeclaration(f) if !f.declare => Some(&**f),
            Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
                if let Some(oxc_ast::ast::Declaration::FunctionDeclaration(f)) = &decl.declaration
                    && !f.declare
                {
                    Some(&**f)
                } else {
                    None
                }
            }
            Statement::ExportDefaultDeclaration(decl) => {
                if let oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) =
                    &decl.declaration
                    && !f.declare
                {
                    Some(&**f)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(f) = f
            && let Some(id) = &f.id
        {
            out.push(id.name.as_str().to_string());
        }
    }
    out
}

/// §16.2.1 — reject `ImportDeclaration` / `ExportDeclaration` in any
/// position other than directly under `ModuleBody`. Top-level
/// occurrences are kept; nested ones (inside a `Block`, `IfStatement`,
/// loop body, switch case, labeled statement, try/catch/finally,
/// function or class body, …) produce a `SyntaxError`.
///
/// # See also
/// - <https://tc39.es/ecma262/#prod-ModuleItem>
/// - <https://tc39.es/ecma262/#sec-module-semantics-static-semantics-early-errors>
fn validate_module_item_positions(program: &Program<'_>) -> Result<(), CompileError> {
    use oxc_ast_visit::Visit;

    struct ModuleItemFinder {
        found: Option<(u32, u32, &'static str)>,
    }
    impl<'a> Visit<'a> for ModuleItemFinder {
        fn visit_import_declaration(&mut self, it: &oxc_ast::ast::ImportDeclaration<'a>) {
            if self.found.is_none() {
                self.found = Some((it.span.start, it.span.end, "import"));
            }
        }
        fn visit_export_named_declaration(
            &mut self,
            it: &oxc_ast::ast::ExportNamedDeclaration<'a>,
        ) {
            if self.found.is_none() {
                self.found = Some((it.span.start, it.span.end, "export"));
            }
        }
        fn visit_export_default_declaration(
            &mut self,
            it: &oxc_ast::ast::ExportDefaultDeclaration<'a>,
        ) {
            if self.found.is_none() {
                self.found = Some((it.span.start, it.span.end, "export"));
            }
        }
        fn visit_export_all_declaration(&mut self, it: &oxc_ast::ast::ExportAllDeclaration<'a>) {
            if self.found.is_none() {
                self.found = Some((it.span.start, it.span.end, "export"));
            }
        }
    }

    for stmt in &program.body {
        if matches!(
            stmt,
            Statement::ImportDeclaration(_)
                | Statement::ExportNamedDeclaration(_)
                | Statement::ExportDefaultDeclaration(_)
                | Statement::ExportAllDeclaration(_)
        ) {
            continue;
        }
        let mut finder = ModuleItemFinder { found: None };
        finder.visit_statement(stmt);
        if let Some((_, _, kind)) = finder.found {
            return Err(CompileError::Syntax {
                messages: vec![format!(
                    "SyntaxError: `{kind}` declarations may only appear at the top level of a module"
                )],
                diagnostics: Vec::new(),
            });
        }
    }
    Ok(())
}

/// Compile a parsed ES module into the frozen runtime boundary product.
///
/// The returned metadata owns source spans plus import/export/live-binding
/// information for the original module fragment. Runtime linkers may merge the
/// bytecode payload, but they should keep this metadata as the per-source
/// diagnostics record.
///
/// # Errors
/// Returns [`CompileError`] when parsing or lowering fails.
/// Compile an already parsed ES-module program into the frozen runtime boundary
/// product.
///
/// Metadata collection and bytecode lowering consume the same borrowed OXC AST.
/// Runtime module loading uses this API after dependency scanning so module
/// compilation does not reparse source.
///
/// # Errors
/// Returns [`CompileError`] when lowering fails.
pub fn compile_module_program_to_module(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    host: &ModuleHostInfo,
) -> Result<CompiledModule, CompileError> {
    let module_metadata = collect_module_metadata(program, host);
    let bytecode = compile_module_program(program, source_kind, host)?;
    let mut metadata = CompiledModuleMetadata::span_only_from_bytecode_with_source(
        &bytecode,
        &host.module_url,
        bytecode_source_kind(source_kind),
    )?;
    metadata.imports = module_metadata.imports;
    metadata.exports = module_metadata.exports;
    metadata.live_binding_slots = module_metadata.live_binding_slots;
    metadata.named_imports = module_metadata.named_imports;
    Ok(CompiledModule::new(bytecode, metadata))
}

/// Compile the inner declaration of an `export <decl>` statement
/// (`export let x = …`, `export function f() {…}`,
/// `export class C {…}`). Mirrors the matching `compile_statement`
/// arms without re-wrapping into a `Statement` — re-wrapping
/// requires arena allocation that isn't available here.
///
/// The pre-pass already added each declared name to
/// `module_state.exported_names`, so the regular store paths
/// emit the `module_env` mirror automatically.
pub(crate) fn compile_export_inner_declaration(
    cx: &mut Compiler,
    decl: &oxc_ast::ast::Declaration<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    match decl {
        oxc_ast::ast::Declaration::VariableDeclaration(v) => {
            let is_const = matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Const);
            let is_var = matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Var);
            for declarator in &v.declarations {
                let dspan = (declarator.span.start, declarator.span.end);
                let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id else {
                    let init = declarator.init.as_ref().ok_or(CompileError::Unsupported {
                        node: "export destructuring requires an initializer".to_string(),
                        span: dspan,
                    })?;
                    let init_reg = compile_expr(cx, init, dspan)?;
                    if is_var {
                        destructure_assign(cx, init_reg, &declarator.id, dspan)?;
                    } else {
                        destructure_into(cx, init_reg, &declarator.id, dspan)?;
                    }
                    continue;
                };
                let name = id.name.as_str().to_string();
                // §16.2.3.7 ExportEntry: `export var x` reuses the
                // module-scope binding pre-hoisted at module entry
                // (var-hoist); `export let x` / `export const x`
                // were pre-declared at module entry by
                // `hoist_lexical_names` so inner functions resolve
                // them. Reuse the pre-declared binding when
                // present; fall back to a fresh declaration only
                // for the foundation cases the lexical hoist pass
                // doesn't yet cover (e.g. destructuring leaves
                // declared at their source position).
                let storage = if is_var {
                    cx.lookup_binding(&name)
                        .ok_or(CompileError::Unsupported {
                            node: format!("export var `{name}` not pre-hoisted"),
                            span: dspan,
                        })?
                        .storage
                } else if let Some(info) = cx.lookup_in_current_scope(&name) {
                    info.storage
                } else {
                    cx.declare_binding(&name, lexical_kind(is_const), dspan)?
                };
                let init_reg = match &declarator.init {
                    Some(init) => compile_expr(cx, init, dspan)?,
                    None => {
                        let dst = cx.alloc_scratch();
                        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], dspan);
                        dst
                    }
                };
                cx.emit_store_storage(init_reg, storage, dspan);
                cx.mark_initialized(&name);
                cx.emit_module_export_mirror(&name, init_reg, dspan);
            }
            Ok(())
        }
        oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
            let fspan = (f.span.start, f.span.end);
            let name =
                f.id.as_ref()
                    .ok_or(CompileError::Unsupported {
                        node: "export function without name".to_string(),
                        span: fspan,
                    })?
                    .name
                    .as_str()
                    .to_string();
            // §10.2.11 step 30 — top-level `function` decls were
            // hoisted at scope entry by
            // `hoist_function_declarations` (now also for the
            // export-wrapped form). The hoist pass already
            // compiled the body and bound the closure; the
            // source-position arm becomes a pure no-op.
            if cx.hoisted_function_names.contains(&name) {
                return Ok(());
            }
            let storage = match cx.lookup_in_current_scope(&name) {
                Some(info) => info.storage,
                None => cx.declare_binding(&name, otter_bytecode::SlotKind::FunctionDecl, fspan)?,
            };
            let record = compile_function_full(
                cx,
                &name,
                &f.params,
                &f.body,
                fspan,
                f.r#async,
                f.generator,
                false,
            )?;
            let tmp = cx.alloc_scratch();
            emit_make_callable(cx, tmp, &record, fspan);
            cx.emit_store_storage(tmp, storage, fspan);
            cx.mark_initialized(&name);
            cx.emit_module_export_mirror(&name, tmp, fspan);
            Ok(())
        }
        oxc_ast::ast::Declaration::ClassDeclaration(c) => {
            let cspan = (c.span.start, c.span.end);
            let name =
                c.id.as_ref()
                    .ok_or(CompileError::Unsupported {
                        node: "export class without name".to_string(),
                        span: cspan,
                    })?
                    .name
                    .as_str()
                    .to_string();
            let class_reg = compile_class(cx, c, Some(&name))?;
            // `export class C` was pre-declared by
            // `hoist_lexical_names` (TDZ-init). The source-
            // position arm only stores the resolved class value
            // and flips the binding to initialized.
            let storage = if let Some(info) = cx.lookup_in_current_scope(&name) {
                info.storage
            } else {
                cx.declare_binding(&name, otter_bytecode::SlotKind::Let, cspan)?
            };
            cx.emit_store_storage(class_reg, storage, cspan);
            cx.mark_initialized(&name);
            cx.emit_module_export_mirror(&name, class_reg, cspan);
            Ok(())
        }
        _ => Err(CompileError::Unsupported {
            node: "ExportNamedDeclaration: non-runtime inner declaration".to_string(),
            span,
        }),
    }
}
