//! Public and internal entry points for script and module compilation.
//!
//! # Contents
//! - source parsing entry points
//! - borrowed AST lowering
//! - module metadata assembly
//! - export declaration lowering
//!
//! # Invariants
//! - The first function record is the entry function for the produced bytecode module.
//!
//! # See also
//! - `errors` and `module_state`

use crate::*;

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

/// One caller-environment binding a direct eval body can see. Slot
/// `i` of the caller-scope list maps to upvalue slot `i` of the
/// compiled `<main>`; the runtime splices the caller's cells into
/// those slots before running the chunk.
#[derive(Debug, Clone)]
pub struct EvalCallerBinding {
    /// Source-level binding name.
    pub name: String,
    /// `true` for `let` / `const` / `class` caller bindings — a
    /// sloppy eval body var-declaring the same name is a runtime
    /// `SyntaxError` (§19.2.1.3 step 5).
    pub lexical: bool,
}

/// Compile an `eval` / `new Function` body. Differs from script
/// compilation in two details: a *strict* eval body gets its own
/// variable environment (§19.2.1.1), so top-level `var` / `function`
/// declarations don't mirror onto the global object; and when the
/// direct-eval call site's variable environment binds `arguments`
/// (`forbid_var_arguments`), a sloppy body var-declaring `arguments`
/// is an early SyntaxError (§19.2.1.3 EvalDeclarationInstantiation).
///
/// `caller_scope` carries the caller variable environment of a
/// direct eval running inside a function: each binding maps to a
/// reserved leading upvalue slot of the produced `<main>`, sloppy
/// `var` / function declarations matching a caller binding reuse the
/// caller's cell, and new var-scoped names are reported back through
/// the `<main>`'s `direct_eval_bindings` table for the runtime to
/// adopt into the caller frame.
///
/// # Errors
/// Returns [`CompileError`] when parsing fails or lowering rejects the AST.
pub fn compile_eval_source(
    source: &str,
    kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
    forbid_var_arguments: bool,
    caller_scope: Option<&[EvalCallerBinding]>,
) -> Result<BytecodeModule, CompileError> {
    // §19.2.1.1 PerformEval parses the body with the Script goal.
    otter_syntax::with_program_goal(source, kind, otter_syntax::SourceGoal::Script, |program| {
        if forbid_var_arguments && !(force_strict || program.has_use_strict_directive()) {
            let mut var_names: Vec<String> = Vec::new();
            hoist_var_names(&program.body, &mut var_names);
            if var_names.iter().any(|name| name == "arguments") {
                return Err(CompileError::Unsupported {
                    node: "SyntaxError: eval body may not var-declare 'arguments' here".to_string(),
                    span: (program.span.start, program.span.end),
                });
            }
        }
        compile_program_for_eval(program, kind, module_specifier, force_strict, caller_scope)
    })
    .map_err(CompileError::from)?
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
    Ok(CompiledModule::from_bytecode(bytecode))
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
    compile_program_with_mode(
        program,
        source_kind,
        module_specifier,
        force_strict,
        false,
        None,
    )
}

pub(crate) fn compile_program_for_eval(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
    caller_scope: Option<&[EvalCallerBinding]>,
) -> Result<BytecodeModule, CompileError> {
    compile_program_with_mode(
        program,
        source_kind,
        module_specifier,
        force_strict,
        true,
        caller_scope,
    )
}

pub(crate) fn compile_program_with_mode(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
    eval_mode: bool,
    caller_scope: Option<&[EvalCallerBinding]>,
) -> Result<BytecodeModule, CompileError> {
    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    let script_module_url = if module_specifier.starts_with("file://") {
        module_specifier.to_string()
    } else {
        Default::default()
    };
    // §16.2.1.7 — top-level `await` upgrades `<main>` to async so
    // the dispatch loop's async machinery parks / resumes the
    // entry frame on suspension points.
    // §12.9.3.1 + §15.7 strict-mode early errors that oxc_parser
    // does not flag on its own (legacy octal / non-octal-decimal
    // integer literals, etc.).
    strict_validation::validate_strict_mode_early_errors(program, force_strict)?;
    // §14.2.1 / §14.12.1 block-level lexical early errors (duplicate
    // LexicallyDeclaredNames, lexical/var clashes) with the Annex B
    // §B.3.3.1 sloppy-mode plain-function exemption.
    strict_validation::validate_block_early_errors(program, force_strict)?;
    let main_is_async = module_body_uses_top_level_await(&program.body);
    let main_is_strict = force_strict || program.has_use_strict_directive();
    // Reserve slot 0 for `<main>` so nested function compilation
    // can pre-register their ids deterministically (slice 13 only
    // needs the immediate id, but the slot reservation keeps the
    // table densely populated).
    module.borrow_mut().functions.push(Function {
        id: 0,
        name: "<main>".to_string(),
        span: (program.span.start, program.span.end),
        is_async: main_is_async,
        is_strict: main_is_strict,
        module_url: script_module_url.clone(),
        ..Default::default()
    });
    let mut top = FunctionContext::new(Rc::clone(&module))
        .with_strict(main_is_strict)
        .with_module_url(script_module_url);
    top.captured_names = capture::analyze_module(&program.body);

    // §19.2.1.3 EvalDeclarationInstantiation — direct eval inside a
    // function. The caller's bindings occupy the leading own-upvalue
    // slots (the runtime splices the caller's cells in); the body's
    // own var-scoped names are forced into cells so the caller can
    // adopt them after the eval returns.
    let caller = caller_scope.unwrap_or(&[]);
    let caller_slot_count = u16::try_from(caller.len()).expect("eval caller scope too large");
    let caller_names: HashSet<&str> = caller.iter().map(|b| b.name.as_str()).collect();
    if !caller.is_empty() && !main_is_strict {
        // Step 5 — a sloppy body var-scoped name colliding with a
        // caller lexical binding is a SyntaxError thrown at the eval
        // call site.
        let caller_lexical: HashSet<&str> = caller
            .iter()
            .filter(|b| b.lexical)
            .map(|b| b.name.as_str())
            .collect();
        let mut body_var_names: Vec<String> = Vec::new();
        hoist_var_names(&program.body, &mut body_var_names);
        body_var_names.extend(crate::annex_b::collect_annex_b_candidates(
            &program.body,
            &HashSet::new(),
        ));
        if let Some(name) = body_var_names
            .iter()
            .find(|name| caller_lexical.contains(name.as_str()))
        {
            return Err(CompileError::Unsupported {
                node: format!("SyntaxError: Identifier '{name}' has already been declared"),
                span: (program.span.start, program.span.end),
            });
        }
        for name in body_var_names {
            if !caller_names.contains(name.as_str()) {
                top.captured_names.insert(name);
            }
        }
    }
    if !caller.is_empty() && capture::program_contains_direct_eval(&program.body) {
        // A nested direct eval sees this chunk's scope as *its*
        // caller environment — promote every body-level binding to a
        // cell so the inner chunk can splice them.
        top.captured_names
            .extend(capture::all_program_names(&program.body));
    }
    top.own_upvalue_count = caller_slot_count;

    let mut cx = Compiler::new(top);
    cx.suppress_global_mirror = eval_mode && (main_is_strict || !caller.is_empty());
    cx.in_eval = eval_mode;
    // Unresolved names may hit bindings a nested direct eval
    // introduces into this chunk's own frame at runtime.
    cx.contains_direct_eval = !caller.is_empty();
    cx.enter_scope();

    if !caller.is_empty() {
        // Strict eval owns its variable environment (§19.2.1.1) —
        // a body var name shadows the caller binding with a fresh
        // local instead of writing through the caller's cell.
        let shadowed: HashSet<String> = if main_is_strict {
            let mut names: Vec<String> = Vec::new();
            hoist_var_names(&program.body, &mut names);
            names.into_iter().collect()
        } else {
            HashSet::new()
        };
        for (slot, binding) in caller.iter().enumerate() {
            if shadowed.contains(&binding.name) {
                continue;
            }
            cx.scopes[0].bindings.insert(
                binding.name.clone(),
                BindingInfo {
                    storage: BindingStorage::Upvalue { idx: slot as u16 },
                    is_const: false,
                    initialized: true,
                },
            );
        }
    }

    // §16.1.7 GlobalDeclarationInstantiation step 16 / §19.2.1.3
    // EvalDeclarationInstantiation step 16.a — script global code and
    // sloppy global-caller eval code create their top-level `var` /
    // function bindings on the global object's environment record,
    // not as `<main>` locals: nested functions, sibling scripts, and
    // eval chunks all resolve the same property, so no reader can
    // observe a stale local copy. Script bindings are
    // non-configurable; eval bindings are deletable. Strict eval and
    // function-caller eval keep the local / caller-cell model.
    let program_span = (program.span.start, program.span.end);
    let mut top_level_vars: Vec<String> = Vec::new();
    hoist_var_names(&program.body, &mut top_level_vars);
    let global_var_bindings = !eval_mode || (caller.is_empty() && !main_is_strict);
    if global_var_bindings {
        cx.script_global_vars = top_level_vars.iter().cloned().collect();
        // §16.1.7 steps 1–12 / §19.2.1.3 steps 5–11 — validate every
        // declared name before any binding is created so a failing
        // script instantiates nothing: lexicals first, then function
        // declarations, then plain vars.
        let function_names: HashSet<String> = top_level_hoistable_function_names(&program.body)
            .into_iter()
            .collect();
        let mut validate_lex: Vec<(String, bool)> = Vec::new();
        if !eval_mode {
            hoist_lexical_names(&program.body, &mut validate_lex);
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
    } else {
        pre_declare_var_bindings(&mut cx, &top_level_vars, program_span)?;
    }
    // §B.3.3.2/3 — sloppy script / eval bodies extend the variable
    // scope with block-level function declaration names.
    pre_declare_annex_b_functions(
        &mut cx,
        &program.body,
        &std::collections::HashSet::new(),
        program_span,
    )?;
    // §10.2.11 step 33 — pre-declare top-level `let` / `const` /
    // `class` names with TDZ so the function-hoist pass below can
    // see them when an inner function captures one of these
    // forward references. Script global code instead declares them
    // on the realm's global declarative record (§16.1.7 step 15) so
    // sibling scripts and eval chunks resolve the same binding; eval
    // lexicals stay private to the eval body (§19.2.1.1).
    let mut top_level_lex: Vec<(String, bool)> = Vec::new();
    hoist_lexical_names(&program.body, &mut top_level_lex);
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
    // §10.2.11 step 30 — top-level `function f() {…}` declarations
    // hoist to the script scope so calls before the source-level
    // declaration resolve to the function value. In global-binding
    // mode this runs *before* the var pre-pass per §16.1.7 steps
    // 16–17 / §19.2.1.3 steps 14–15: a CanDeclareGlobalFunction
    // TypeError must abort before any var binding is created.
    hoist_function_declarations(&mut cx, &program.body)?;
    if global_var_bindings {
        let function_names: HashSet<String> = top_level_hoistable_function_names(&program.body)
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

    let mut last_value_reg: Option<u16> = None;
    // A directive prologue (`"use strict"`, etc.) is a sequence of
    // string-literal expression statements; each contributes its
    // string value to the script / `eval` completion value (so
    // `eval('"x"')` evaluates to `"x"`). oxc lifts these out of
    // `body` into `directives`, so emit them here first.
    for directive in &program.directives {
        let dst = cx.alloc_scratch();
        let const_idx = cx.intern_string_constant(&directive.expression.value);
        cx.emit(
            Op::LoadString,
            [Operand::Register(dst), Operand::ConstIndex(const_idx)],
            (directive.span.start, directive.span.end),
        );
        last_value_reg = Some(dst);
    }
    for stmt in &program.body {
        if let Some(reg) = compile_statement(&mut cx, stmt)? {
            last_value_reg = Some(reg);
        }
    }
    // The chunk's full cell-backed scope table. The runtime adopts
    // the var-shaped entries that were not part of the caller scope
    // into the caller frame (§19.2.1.3 step 16.b); the whole table
    // doubles as the caller environment for a nested direct eval
    // running from this chunk's frame.
    let mut eval_new_bindings: Vec<otter_bytecode::DirectEvalBinding> = Vec::new();
    if !caller.is_empty() {
        let body_lexical: HashSet<&str> = top_level_lex
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        let caller_lexical: HashSet<&str> = caller
            .iter()
            .filter(|binding| binding.lexical)
            .map(|binding| binding.name.as_str())
            .collect();
        if let Some(scope) = cx.scopes.first() {
            for (name, info) in &scope.bindings {
                if let BindingStorage::Upvalue { idx } = info.storage {
                    eval_new_bindings.push(otter_bytecode::DirectEvalBinding {
                        name: name.clone(),
                        upvalue: idx,
                        // A strict eval's own variable environment is
                        // private (§19.2.1.1) — flagging every entry
                        // non-adoptable keeps the bindings visible to
                        // nested evals without leaking them into the
                        // caller frame.
                        lexical: main_is_strict
                            || body_lexical.contains(name.as_str())
                            || caller_lexical.contains(name.as_str()),
                    });
                }
            }
        }
        eval_new_bindings.sort_by(|a, b| a.name.cmp(&b.name));
    }
    cx.exit_scope();

    // Synthesize the program's completion value. If the body
    // produced one, return it; otherwise materialize `undefined` in
    // r0 and return that.
    let return_reg = match last_value_reg {
        Some(reg) => reg,
        None => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadUndefined,
                [Operand::Register(dst)],
                (program.span.start, program.span.end),
            );
            dst
        }
    };
    let span = (program.span.start, program.span.end);
    cx.emit(Op::Return, [Operand::Register(return_reg)], span);

    // Finalize `<main>` into the module's function table, then
    // drop `cx` so the module Rc has a single owner before
    // `try_unwrap`.
    {
        let mut m = module.borrow_mut();
        m.functions[0].locals = 0;
        m.functions[0].scratch = cx.scratch;
        m.functions[0].own_upvalue_count = cx.own_upvalue_count;
        m.functions[0].direct_eval_bindings = eval_new_bindings;
        m.functions[0].code = std::mem::take(&mut cx.code);
        m.functions[0].spans = std::mem::take(&mut cx.spans);
    }
    drop(cx);

    let kind = bytecode_source_kind(source_kind);

    let ModuleBuilder {
        functions,
        constants,
        next_private_namespace: _,
    } = Rc::try_unwrap(module)
        .expect("module builder should be uniquely owned at finalize")
        .into_inner();

    Ok(BytecodeModule {
        module: module_specifier.to_string(),
        source_kind: kind,
        functions,
        constants,
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
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
/// 1. Run an import pre-pass over the program body, registering a
///    fresh `import_record_<n>` upvalue per source specifier and
///    recording each importer-side alias → `(record_uv, source_name)`
///    binding (§16.2.2 ModuleNamespaceObject for `import * as`,
///    §16.2.3 ImportEntry for named imports).
/// 2. Run an export pre-pass to collect the names this module
///    exports (§16.2.3 ExportEntry). Every later assignment to one
///    of those names emits an extra `StoreProperty module_env,
///    name, value` so live bindings propagate across modules.
/// 3. Allocate own-upvalue cells for `module_env` (param 0) and
///    `import_meta` (param 1), hoist the parameters into those
///    cells at function entry so closures defined inside the body
///    can capture them via the regular upvalue mechanism.
/// 4. Allocate one own-upvalue cell per import source and emit
///    `Op::ImportNamespace cell_dst, specifier_const` followed by
///    a `StoreUpvalue` to populate it. Subsequent reads of an
///    imported alias resolve through this cell.
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
    strict_validation::validate_strict_mode_early_errors(program, true)?;
    // §14.2.1 / §14.12.1 block-level lexical early errors; module
    // code is always strict so no Annex B exemption applies.
    strict_validation::validate_block_early_errors(program, true)?;
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
    top.captured_names = capture::analyze_module(&program.body);
    // Hoisted function declarations instantiate during the link-phase
    // init invocation; the evaluation-phase invocation (a separate
    // frame) must observe the same closure values, so their bindings
    // are forced into the persistent own-upvalue cells.
    for name in top_level_hoistable_function_names(&program.body) {
        top.captured_names.insert(name);
    }
    // Also capture names that any inner function references whose
    // bindings live as `module_env` / `import_meta` / `import_record_*`
    // — those are forced own-upvalues (see allocate_module_upvalues).

    // Allocate own-upvalues for module_env, import_meta, and
    // every imported source. These slots must be stable for the
    // body's compilation so we reserve them up front.
    let module_env_uv = top.own_upvalue_count;
    top.own_upvalue_count = top.own_upvalue_count.checked_add(1).expect("uv overflow");
    let import_meta_uv = top.own_upvalue_count;
    top.own_upvalue_count = top.own_upvalue_count.checked_add(1).expect("uv overflow");

    let mut state = ModuleState {
        module_env_uv,
        import_meta_uv,
        ..ModuleState::default()
    };
    state.pre_resolved_imports = host.resolved_imports.clone();

    // Pre-pass: collect import sources + record per-source upvalue
    // slots; collect exported names + import bindings.
    let mut import_sources_in_order: Vec<String> = Vec::new();
    let mut deferred_sources_in_order: Vec<String> = Vec::new();
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
                let specifier = decl.source.value.as_str().to_string();
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
                // Deferred imports bind to a dedicated cell so they are
                // not pulled into the eager-evaluation set and stay
                // distinct from any eager namespace of the same module.
                let record_uv = if is_defer_phase {
                    if let Some(&uv) = state.deferred_import_records.get(&specifier) {
                        uv
                    } else {
                        let uv = top.own_upvalue_count;
                        top.own_upvalue_count =
                            top.own_upvalue_count.checked_add(1).expect("uv overflow");
                        state.deferred_import_records.insert(specifier.clone(), uv);
                        deferred_sources_in_order.push(specifier.clone());
                        uv
                    }
                } else {
                    if !state.import_records.contains_key(&specifier) {
                        let uv = top.own_upvalue_count;
                        top.own_upvalue_count =
                            top.own_upvalue_count.checked_add(1).expect("uv overflow");
                        state.import_records.insert(specifier.clone(), uv);
                        import_sources_in_order.push(specifier.clone());
                    }
                    state.import_records[&specifier]
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
                                        record_uv_idx: record_uv,
                                        source_name,
                                        is_namespace: false,
                                        specifier: specifier.clone(),
                                        is_deferred: is_defer_phase,
                                    },
                                );
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                let alias = s.local.name.as_str().to_string();
                                state.imported_names.insert(
                                    alias,
                                    ImportBinding {
                                        record_uv_idx: record_uv,
                                        source_name: "default".to_string(),
                                        is_namespace: false,
                                        specifier: specifier.clone(),
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
                                        record_uv_idx: record_uv,
                                        source_name: String::new(),
                                        is_namespace: true,
                                        specifier: specifier.clone(),
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
                    let specifier = source.value.as_str().to_string();
                    if !state.import_records.contains_key(&specifier) {
                        let uv = top.own_upvalue_count;
                        top.own_upvalue_count =
                            top.own_upvalue_count.checked_add(1).expect("uv overflow");
                        state.import_records.insert(specifier.clone(), uv);
                        import_sources_in_order.push(specifier);
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
                    .map(|s| s.value.as_str())
                    .and_then(|spec| host.resolved_imports.get(spec))
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
                let specifier = decl.source.value.as_str().to_string();
                if !state.import_records.contains_key(&specifier) {
                    let uv = top.own_upvalue_count;
                    top.own_upvalue_count =
                        top.own_upvalue_count.checked_add(1).expect("uv overflow");
                    state.import_records.insert(specifier.clone(), uv);
                    import_sources_in_order.push(specifier);
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

    top.module_state = Some(state);

    let mut cx = Compiler::new(top);
    cx.enter_scope();

    // Register synthetic bindings for module_env and each
    // import_record so inner functions can capture them through
    // the regular `resolve_capture` cascade. Without these, an
    // inner function that references an imported alias would see
    // no binding in its scope chain and fail with
    // "unresolved identifier".
    {
        let env_uv_sb = cx.module_state.as_ref().unwrap().module_env_uv;
        let meta_uv_sb = cx.module_state.as_ref().unwrap().import_meta_uv;
        let record_uvs: Vec<u16> = {
            let ms = cx.module_state.as_ref().unwrap();
            ms.import_records
                .values()
                .chain(ms.deferred_import_records.values())
                .copied()
                .collect()
        };
        cx.scopes[0].bindings.insert(
            module_env_synthetic_name(),
            BindingInfo {
                storage: BindingStorage::Upvalue { idx: env_uv_sb },
                is_const: true,
                initialized: true,
            },
        );
        cx.scopes[0].bindings.insert(
            import_meta_synthetic_name(),
            BindingInfo {
                storage: BindingStorage::Upvalue { idx: meta_uv_sb },
                is_const: true,
                initialized: true,
            },
        );
        for uv in &record_uvs {
            cx.scopes[0].bindings.insert(
                import_record_synthetic_name(*uv),
                BindingInfo {
                    storage: BindingStorage::Upvalue { idx: *uv },
                    is_const: true,
                    initialized: true,
                },
            );
        }
    }

    // Hoist params into upvalue cells: param 0 → module_env_uv,
    // param 1 → import_meta_uv. The runtime stores params in
    // registers 0..param_count; we re-emit StoreUpvalue from those.
    let span0 = (program.span.start, program.span.end);
    let env_uv = cx.module_state.as_ref().unwrap().module_env_uv;
    let meta_uv = cx.module_state.as_ref().unwrap().import_meta_uv;
    cx.emit(
        Op::StoreUpvalue,
        [Operand::Register(0), Operand::Imm32(env_uv as i32)],
        span0,
    );
    cx.emit(
        Op::StoreUpvalue,
        [Operand::Register(1), Operand::Imm32(meta_uv as i32)],
        span0,
    );
    cx.scratch = 3; // params occupy r0 (env), r1 (meta), r2 (link-phase flag)

    // For each import source, emit Op::ImportNamespace then
    // StoreUpvalue to populate the per-source record cell.
    for specifier in &import_sources_in_order {
        let record_uv = cx.module_state.as_ref().unwrap().import_records[specifier];
        let scratch = cx.alloc_scratch();
        let spec_const = cx.intern_string_constant(specifier);
        cx.emit(
            Op::ImportNamespace,
            [Operand::Register(scratch), Operand::ConstIndex(spec_const)],
            span0,
        );
        cx.emit(
            Op::StoreUpvalue,
            [Operand::Register(scratch), Operand::Imm32(record_uv as i32)],
            span0,
        );
    }

    // For each `import defer` source, emit Op::ImportNamespaceDeferred
    // (resolves a deferred namespace object without evaluating the
    // module) then StoreUpvalue into the deferred record cell.
    for specifier in &deferred_sources_in_order {
        let record_uv = cx.module_state.as_ref().unwrap().deferred_import_records[specifier];
        let scratch = cx.alloc_scratch();
        let spec_const = cx.intern_string_constant(specifier);
        cx.emit(
            Op::ImportNamespaceDeferred,
            [Operand::Register(scratch), Operand::ConstIndex(spec_const)],
            span0,
        );
        cx.emit(
            Op::StoreUpvalue,
            [Operand::Register(scratch), Operand::Imm32(record_uv as i32)],
            span0,
        );
    }

    // §16.2.1.7 InitializeEnvironment — the prologue below runs only
    // during the link-phase invocation (third argument truthy); the
    // evaluation-phase invocation jumps straight to the body. Both
    // frames share the module's persistent own-upvalue cells, so the
    // closures and TDZ slots the link phase created stay visible.
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
            let env_uv = cx
                .module_state
                .as_ref()
                .map(|s| s.module_env_uv)
                .expect("module_state present for module fragment");
            let env_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                [Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
                span0,
            );
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
        compile_statement(&mut cx, stmt)?;
    }
    cx.exit_scope();

    cx.emit(Op::ReturnUndefined, [], span0);

    {
        let mut m = module.borrow_mut();
        m.functions[0].locals = 0;
        m.functions[0].scratch = cx.scratch;
        m.functions[0].own_upvalue_count = cx.own_upvalue_count;
        m.functions[0].code = std::mem::take(&mut cx.code);
        m.functions[0].spans = std::mem::take(&mut cx.spans);
    }
    // Capture deferred import specifiers before dropping the compiler
    // so resolution edges can be flagged. A specifier imported both
    // eagerly and via `import defer` counts as eager for reachability
    // (the module evaluates eagerly regardless), so it is excluded.
    let deferred_only_specs: HashSet<String> = {
        let ms = cx.module_state.as_ref();
        let eager: HashSet<&String> = ms
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
        next_private_namespace: _,
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
        .map(|(specifier, target)| otter_bytecode::ModuleResolution {
            referrer: host.module_url.clone(),
            specifier: specifier.clone(),
            target: target.clone(),
            deferred: deferred_only_specs.contains(specifier),
            dynamic: false,
        })
        .collect();

    Ok(BytecodeModule {
        module: host.module_url.clone(),
        source_kind: kind,
        functions,
        constants,
        module_resolutions,
        module_inits: Vec::new(),
    })
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
    let mut metadata = CompiledModuleMetadata::from_bytecode(
        &bytecode,
        host.module_url.clone(),
        bytecode_source_kind(source_kind),
    );
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
                // `hoist_lexical_names` so inner functions could
                // capture them through the standard upvalue
                // cascade. Reuse the pre-declared binding when
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
                    cx.declare_binding(&name, is_const, dspan)?
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
            let (function_id, captures) = compile_function_full(
                cx,
                &name,
                &f.params,
                &f.body,
                fspan,
                f.r#async,
                f.generator,
                false,
            )?;
            let storage = cx.declare_binding(&name, false, fspan)?;
            let const_idx = cx.intern_function_id(function_id);
            let tmp = cx.alloc_scratch();
            emit_make_callable(cx, tmp, const_idx, &captures, false, fspan)?;
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
                cx.declare_binding(&name, false, cspan)?
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
