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
    with_program(source, kind, |program| {
        compile_program(program, kind, module_specifier, force_strict)
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
    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    // §16.2.1.7 — top-level `await` upgrades `<main>` to async so
    // the dispatch loop's async machinery parks / resumes the
    // entry frame on suspension points.
    // §12.9.3.1 + §15.7 strict-mode early errors that oxc_parser
    // does not flag on its own (legacy octal / non-octal-decimal
    // integer literals, etc.).
    strict_validation::validate_strict_mode_early_errors(program, force_strict)?;
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
        ..Default::default()
    });
    let mut top = FunctionContext::new(Rc::clone(&module)).with_strict(main_is_strict);
    top.captured_names = capture::analyze_module(&program.body);
    let mut cx = Compiler::new(top);
    cx.enter_scope();

    // §16.1.7 GlobalDeclarationInstantiation / §16.2.1.7
    // ModuleDeclarationInstantiation step 11: top-level `var`
    // declarations hoist to the script / module scope. Pre-bind
    // them to `undefined` here so reads before the source-level
    // declaration see the hoisted value rather than a TDZ error.
    let program_span = (program.span.start, program.span.end);
    let mut top_level_vars: Vec<String> = Vec::new();
    hoist_var_names(&program.body, &mut top_level_vars);
    pre_declare_var_bindings(&mut cx, &top_level_vars, program_span)?;
    // §10.2.11 step 33 — pre-declare top-level `let` / `const` /
    // `class` names with TDZ so the function-hoist pass below can
    // see them when an inner function captures one of these
    // forward references.
    let mut top_level_lex: Vec<(String, bool)> = Vec::new();
    hoist_lexical_names(&program.body, &mut top_level_lex);
    pre_declare_lexical_bindings(&mut cx, &top_level_lex, program_span)?;
    // §10.2.11 step 30 — top-level `function f() {…}` declarations
    // hoist to the script scope so calls before the source-level
    // declaration resolve to the function value.
    hoist_function_declarations(&mut cx, &program.body)?;

    let mut last_value_reg: Option<u16> = None;
    for stmt in &program.body {
        if let Some(reg) = compile_statement(&mut cx, stmt)? {
            last_value_reg = Some(reg);
        }
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
        param_count: 2, // module_env, import_meta
        ..Default::default()
    });

    let mut top = FunctionContext::new(Rc::clone(&module)).with_strict(true);
    top.captured_names = capture::analyze_module(&program.body);
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
    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(decl) if !decl.import_kind.is_type() => {
                let specifier = decl.source.value.as_str().to_string();
                if !state.import_records.contains_key(&specifier) {
                    let uv = top.own_upvalue_count;
                    top.own_upvalue_count =
                        top.own_upvalue_count.checked_add(1).expect("uv overflow");
                    state.import_records.insert(specifier.clone(), uv);
                    import_sources_in_order.push(specifier.clone());
                }
                let record_uv = state.import_records[&specifier];
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
                            for declarator in &var_decl.declarations {
                                if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) =
                                    &declarator.id
                                {
                                    state.exported_names.insert(id.name.as_str().to_string());
                                }
                            }
                        }
                        oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
                            if let Some(id) = &f.id {
                                state.exported_names.insert(id.name.as_str().to_string());
                            }
                        }
                        oxc_ast::ast::Declaration::ClassDeclaration(c) => {
                            if let Some(id) = &c.id {
                                state.exported_names.insert(id.name.as_str().to_string());
                            }
                        }
                        _ => {}
                    }
                }
                for spec in &decl.specifiers {
                    let exported_name = module_export_name_to_str(&spec.exported);
                    state.exported_names.insert(exported_name);
                }
            }
            Statement::ExportDefaultDeclaration(_) => {
                state.exported_names.insert("default".to_string());
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
        let record_uvs: Vec<u16> = cx
            .module_state
            .as_ref()
            .unwrap()
            .import_records
            .values()
            .copied()
            .collect();
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
    cx.scratch = 2; // params occupy r0, r1

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
    // → (referrer, specifier, target) triple.
    let module_resolutions: Vec<otter_bytecode::ModuleResolution> = host
        .resolved_imports
        .iter()
        .map(|(specifier, target)| otter_bytecode::ModuleResolution {
            referrer: host.module_url.clone(),
            specifier: specifier.clone(),
            target: target.clone(),
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
                    return Err(CompileError::Unsupported {
                        node: "export with destructuring not yet supported".to_string(),
                        span: dspan,
                    });
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
            emit_make_callable(cx, tmp, const_idx, &captures, false, fspan);
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
