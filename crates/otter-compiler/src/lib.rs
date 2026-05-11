//! AST → bytecode lowering with full foundation TS erasure.
//!
//! The compiler walks the OXC AST produced by `otter-syntax` and
//! emits an [`otter_bytecode::BytecodeModule`]. After task 08 the
//! frontend handles the foundation TypeScript subset documented in the
//! mdBook frontend chapter:
//!
//! - **Erased silently** (compile to nothing): `interface`, `type`
//!   aliases, `declare` statements/functions, `import type`,
//!   `export type`, abstract methods.
//! - **Erased through** at the expression layer: `as`, `satisfies`,
//!   non-null `!`, legacy `<T>` type assertion, instantiation
//!   `f<T>` (kept transparent — operand survives).
//! - **Rejected with `TS_UNSUPPORTED` diagnostics**: `enum`,
//!   `namespace` (with runtime members), decorators. These return
//!   [`CompileError::TypeScriptUnsupported`].
//!
//! Code surface accepted at this slice: empty scripts, `undefined;`
//! statements, plus any of the above wrapped around them. Slice
//! tasks `09`–`13` add real value loading, control flow, and calls.
//!
//! # Contents
//! - [`compile`] — entry point.
//! - [`CompileError`] — concrete error enum (`Syntax`,
//!   `TypeScriptUnsupported`, `Unsupported`).
//! - [`unwrap_ts_expr`] — strip TS-erasable expression wrappers.
//!
//! # Invariants
//! - The function table starts with `<main>` at index 0.
//! - Every emitted instruction has a matching `SpanEntry` so source
//!   spans survive into diagnostics and stack traces (foundation
//!   plan §M2).
//! - TypeScript erasure preserves the **original** spans — we never
//!   re-emit JS source and re-parse.
//!
//! # See also
//! - [Frontend and compilation](../../../docs/book/src/engine/frontend.md)

mod capture;
mod compiled_module;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use compiled_module::collect_module_metadata;
pub use compiled_module::{
    CompiledExport, CompiledImport, CompiledImportKind, CompiledModule, CompiledModuleMetadata,
    CompiledSourceSpan, LiveBindingSlot,
};
use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Constant, Function, Instruction,
    MappedArgumentBinding, Op, Operand, SourceKind as BytecodeSourceKind, SpanEntry,
};
use otter_syntax::{
    Parsed, SourceKind as SyntaxSourceKind, SyntaxDiagnostic, SyntaxError, with_program,
};
use oxc_ast::ast::{
    AssignmentOperator, AssignmentTarget, BinaryOperator, Expression, LogicalOperator, Program,
    SimpleAssignmentTarget, Statement, UnaryOperator, UpdateOperator,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Compile-time mode discriminator. Reserved for the public API
/// once embedders gain a single entry point; today the two
/// flavours have separate functions ([`compile`] for scripts,
/// [`compile_module_fragment`] for ES-module fragments).
///
/// Spec: <https://tc39.es/ecma262/#sec-types-of-source-code>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // public-API consolidation is its own slice
pub enum CompileMode {
    /// Plain script. Top-level statements compile into `<main>`'s
    /// body; `import` / `export` declarations are rejected.
    Script,
    /// ES module fragment. Top-level statements compile into a
    /// `<module-init>` whose first two parameters are `module_env`
    /// and `import_meta`, with `is_module = true` set on the
    /// resulting [`Function`]. Import / export declarations
    /// lower against the captured environment objects so live
    /// bindings fall out via `LoadProperty` / `StoreProperty`.
    Module,
}

/// One pre-resolved import-record binding: maps an importer-side
/// alias (`import { a as alias } from "./other.ts"`) to the
/// import-record upvalue index plus the original source-side name
/// the property load reads.
#[derive(Debug, Clone)]
struct ImportBinding {
    /// Own-upvalue index of the `import_record_<n>` JsObject inside
    /// the running `<module-init>` frame.
    record_uv_idx: u16,
    /// Source-module name of the binding (e.g., the `a` in
    /// `import { a as alias } from "./other.ts"`). For default
    /// imports this is `"default"`. For namespace imports the
    /// alias resolves directly to the record itself; we store an
    /// empty string here as the sentinel.
    source_name: String,
    /// `true` for `import * as ns from "./..."` — the alias binds
    /// to the namespace JsObject directly, so reads return the
    /// record without an extra `LoadProperty`.
    is_namespace: bool,
}

/// Module-mode state attached to a [`FunctionContext`] when the
/// function is the top-level `<module-init>` of an ES-module
/// fragment.
#[derive(Debug, Default)]
struct ModuleState {
    /// Own-upvalue index of the `module_env` JsObject (param 0,
    /// hoisted into a cell at the top of the body so closures can
    /// capture it).
    module_env_uv: u16,
    /// Own-upvalue index of the `import_meta` JsObject (param 1).
    import_meta_uv: u16,
    /// Per-specifier upvalue index of the import-record JsObject.
    /// Populated by the import pre-pass at the start of the body.
    import_records: HashMap<String, u16>,
    /// Importer-side alias → import-record binding info.
    imported_names: HashMap<String, ImportBinding>,
    /// Names that this module exports. Every assignment to a name
    /// in this set emits an extra
    /// `StoreProperty module_env, name, value` after the regular
    /// store so live-binding writes propagate.
    exported_names: HashSet<String>,
    /// Per-specifier resolved target URL — populated by the host
    /// before module compilation begins. The compiler emits the
    /// pre-resolved (referrer, specifier, target) triple into the
    /// produced fragment's `module_resolutions` table.
    pre_resolved_imports: HashMap<String, String>,
}

/// Pre-resolved import / export information passed by the host
/// (typically the runtime's module-graph driver) into
/// [`compile_module_fragment`]. The compiler trusts the host for
/// resolution; it lowers identifier references against the
/// resolved structure.
#[derive(Debug, Clone, Default)]
pub struct ModuleHostInfo {
    /// Canonical URL of this module (e.g.,
    /// `"file:///abs/path/to/main.ts"`).
    pub module_url: String,
    /// Specifier → target URL pairs — every specifier the
    /// module references in a static `import` or
    /// literal-string `import("./x")` must be present.
    pub resolved_imports: HashMap<String, String>,
}

/// Compile a parsed program into a [`BytecodeModule`] script.
///
/// `module_specifier` is recorded on the resulting bytecode and
/// surfaces in dump output, traces, and diagnostics.
///
/// # Errors
/// Returns [`CompileError`] when the AST contains constructs outside
/// the foundation subset (see [`CompileError::Unsupported`]).
pub fn compile(parsed: &Parsed, module_specifier: &str) -> Result<BytecodeModule, CompileError> {
    let program = parsed.program().map_err(CompileError::from)?;
    compile_program(&program, parsed.kind, module_specifier, false)
}

/// Compile a parsed script into the frozen runtime boundary product.
///
/// # Errors
/// Returns [`CompileError`] when parsing or lowering fails.
pub fn compile_to_module(
    parsed: &Parsed,
    module_specifier: &str,
) -> Result<CompiledModule, CompileError> {
    let bytecode = compile(parsed, module_specifier)?;
    Ok(CompiledModule::from_bytecode(bytecode))
}

/// Compile source text through a single OXC parse.
///
/// This is the runtime hot path. Use [`compile`] when a caller already owns a
/// [`Parsed`] value for tests or staged analysis.
///
/// # Errors
/// Returns [`CompileError`] when parsing fails or the AST contains constructs
/// outside the foundation subset.
pub fn compile_source(
    source: &str,
    kind: SyntaxSourceKind,
    module_specifier: &str,
) -> Result<BytecodeModule, CompileError> {
    compile_source_with_forced_strict(source, kind, module_specifier, false)
}

/// Compile source text with an optional inherited strict-mode
/// override. Direct eval uses this to model ECMA-262's caller
/// strictness inheritance without rewriting source text.
///
/// # Errors
/// Returns [`CompileError`] when parsing fails or lowering rejects the AST.
pub fn compile_source_with_forced_strict(
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
pub fn compile_source_to_module(
    source: &str,
    kind: SyntaxSourceKind,
    module_specifier: &str,
) -> Result<CompiledModule, CompileError> {
    let bytecode = compile_source(source, kind, module_specifier)?;
    Ok(CompiledModule::from_bytecode(bytecode))
}

/// Compile an already parsed OXC program.
///
/// This keeps callers that need a syntax pass for routing or analysis from
/// parsing the same source twice. The caller must pass the same source kind
/// that was used to create `program`.
///
/// # Errors
/// Returns [`CompileError`] when the AST contains constructs outside the
/// foundation subset.
pub fn compile_parsed_program(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
) -> Result<BytecodeModule, CompileError> {
    compile_program(program, source_kind, module_specifier, false)
}

/// Compile an already parsed OXC program into the frozen runtime boundary
/// product.
///
/// # Errors
/// Returns [`CompileError`] when lowering fails.
pub fn compile_parsed_program_to_module(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
) -> Result<CompiledModule, CompileError> {
    let bytecode = compile_parsed_program(program, source_kind, module_specifier)?;
    Ok(CompiledModule::from_bytecode(bytecode))
}

fn compile_program(
    program: &Program<'_>,
    source_kind: SyntaxSourceKind,
    module_specifier: &str,
    force_strict: bool,
) -> Result<BytecodeModule, CompileError> {
    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    // §16.2.1.7 — top-level `await` upgrades `<main>` to async so
    // the dispatch loop's async machinery parks / resumes the
    // entry frame on suspension points.
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
                vec![Operand::Register(dst)],
                (program.span.start, program.span.end),
            );
            dst
        }
    };
    let span = (program.span.start, program.span.end);
    cx.emit(Op::Return, vec![Operand::Register(return_reg)], span);

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
pub fn compile_module_fragment(
    parsed: &Parsed,
    host: &ModuleHostInfo,
) -> Result<BytecodeModule, CompileError> {
    let program = parsed.program().map_err(CompileError::from)?;

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
        vec![Operand::Register(0), Operand::Imm32(env_uv as i32)],
        span0,
    );
    cx.emit(
        Op::StoreUpvalue,
        vec![Operand::Register(1), Operand::Imm32(meta_uv as i32)],
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
            vec![Operand::Register(scratch), Operand::ConstIndex(spec_const)],
            span0,
        );
        cx.emit(
            Op::StoreUpvalue,
            vec![Operand::Register(scratch), Operand::Imm32(record_uv as i32)],
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

    cx.emit(Op::ReturnUndefined, vec![], span0);

    {
        let mut m = module.borrow_mut();
        m.functions[0].locals = 0;
        m.functions[0].scratch = cx.scratch;
        m.functions[0].own_upvalue_count = cx.own_upvalue_count;
        m.functions[0].code = std::mem::take(&mut cx.code);
        m.functions[0].spans = std::mem::take(&mut cx.spans);
    }
    drop(cx);

    let kind = bytecode_source_kind(parsed.kind);

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
pub fn compile_module_fragment_to_module(
    parsed: &Parsed,
    host: &ModuleHostInfo,
) -> Result<CompiledModule, CompileError> {
    let program = parsed.program().map_err(CompileError::from)?;
    let module_metadata = collect_module_metadata(&program, host);
    let bytecode = compile_module_fragment(parsed, host)?;
    let mut metadata = CompiledModuleMetadata::from_bytecode(
        &bytecode,
        host.module_url.clone(),
        bytecode_source_kind(parsed.kind),
    );
    metadata.imports = module_metadata.imports;
    metadata.exports = module_metadata.exports;
    metadata.live_binding_slots = module_metadata.live_binding_slots;
    Ok(CompiledModule::new(bytecode, metadata))
}

fn bytecode_source_kind(kind: SyntaxSourceKind) -> BytecodeSourceKind {
    if kind.is_typescript() {
        BytecodeSourceKind::TypeScript
    } else {
        BytecodeSourceKind::JavaScript
    }
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
fn compile_export_inner_declaration(
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
                        cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], dspan);
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

/// Synthetic binding name used to capture the `module_env`
/// JsObject through inner-function `resolve_capture` cascades.
/// Inner functions that mutate a module-level export reach the
/// outer module-init's `module_env` cell via this name.
fn module_env_synthetic_name() -> String {
    "__otter_module_env".to_string()
}

/// Synthetic binding name for the `import_meta` JsObject.
fn import_meta_synthetic_name() -> String {
    "__otter_import_meta".to_string()
}

/// Synthetic binding name for an import-record at the given
/// outer-frame upvalue index. Distinct names per-record let inner
/// functions cascade each independently.
fn import_record_synthetic_name(record_uv: u16) -> String {
    format!("__otter_import_record_{record_uv}")
}

/// Find the deepest [`ModuleState`] frame that declares an
/// imported alias matching `name`. Returns the binding info
/// Whether `name` is one of the eleven canonical TypedArray
/// constructor names per ECMA-262 Table 71. Used by the compiler to
/// intercept `new <T>(...)` / `<T>.from(...)` / `<T>.of(...)` and
/// route through `Op::TypedArrayCall`.
fn is_typed_array_name(name: &str) -> bool {
    matches!(
        name,
        "Int8Array"
            | "Uint8Array"
            | "Uint8ClampedArray"
            | "Int16Array"
            | "Uint16Array"
            | "Int32Array"
            | "Uint32Array"
            | "Float32Array"
            | "Float64Array"
            | "BigInt64Array"
            | "BigUint64Array"
    )
}

/// alongside the synthetic upvalue name the inner function should
/// resolve via `resolve_capture` to land at the same record cell.
fn find_module_import_binding(cx: &Compiler, name: &str) -> Option<(ImportBinding, String)> {
    for frame in cx.stack.iter().rev() {
        if let Some(state) = &frame.module_state
            && let Some(binding) = state.imported_names.get(name)
        {
            return Some((
                binding.clone(),
                import_record_synthetic_name(binding.record_uv_idx),
            ));
        }
    }
    None
}

/// Stringify an OXC `ModuleExportName` (used by named-export
/// specifiers). The three variants are:
/// - `IdentifierName` (`export { a }`),
/// - `IdentifierReference` (`export { a as b }`'s `a`),
/// - `StringLiteral` (`export { "a" as b }`).
pub(crate) fn module_export_name_to_str(name: &oxc_ast::ast::ModuleExportName<'_>) -> String {
    match name {
        oxc_ast::ast::ModuleExportName::IdentifierName(id) => id.name.as_str().to_string(),
        oxc_ast::ast::ModuleExportName::IdentifierReference(id) => id.name.as_str().to_string(),
        oxc_ast::ast::ModuleExportName::StringLiteral(lit) => lit.value.as_str().to_string(),
    }
}

/// Decode oxc's lossy lone-surrogate encoding back into raw WTF-16
/// code units. When `StringLiteral::lone_surrogates` is set, oxc
/// stores each lone surrogate as `\u{FFFD}XXXX` (four lowercase hex
/// digits) and the literal U+FFFD as `\u{FFFD}fffd`. This decoder
/// reverses both encodings so the runtime sees the source-fidelity
/// code units expected by §6.1.4
/// [`The String Type`](https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type).
fn decode_lone_surrogate_string(value: &str) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(value.len());
    let mut iter = value.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\u{FFFD}' {
            // Followed by four lowercase hex digits encoding a u16.
            let mut hex = [0u8; 4];
            let mut count = 0;
            for slot in &mut hex {
                match iter.peek() {
                    Some(&h) if h.is_ascii_hexdigit() => {
                        *slot = h as u8;
                        iter.next();
                        count += 1;
                    }
                    _ => break,
                }
            }
            if count == 4 {
                let s = std::str::from_utf8(&hex).unwrap();
                let unit = u16::from_str_radix(s, 16).unwrap();
                out.push(unit);
                continue;
            }
            // Malformed (shouldn't happen if `lone_surrogates`
            // signal is honoured) — fall back to literal U+FFFD.
            out.push(0xFFFD);
            for h in &hex[..count] {
                out.push(*h as u16);
            }
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf).iter() {
                out.push(*u);
            }
        }
    }
    out
}

/// Module-level mutable state shared across nested function
/// compilations. Threaded as `Rc<RefCell<ModuleBuilder>>` so the
/// `<main>` context and any nested function context can intern
/// constants into the same pool and register their `Function`
/// records into the same table without contorting the borrow
/// checker.
#[derive(Debug, Default)]
struct ModuleBuilder {
    functions: Vec<Function>,
    constants: Vec<Constant>,
    /// Monotonic counter handed out by `compile_class` so each
    /// lexical class declaration owns a private-field namespace
    /// distinct from every other class — `class A { #x }` and
    /// `class B { #x }` mangle to different runtime keys, matching
    /// §15.7.1 PrivateName uniqueness.
    next_private_namespace: u32,
}

/// One lexical scope's binding table. The compiler keeps a stack
/// of these so block-scoped `let`/`const` shadow correctly.
#[derive(Debug, Default)]
struct Scope {
    /// Map from binding name → register index (locals + scratch
    /// share one window in the foundation slice; locals occupy the
    /// low end).
    bindings: HashMap<String, BindingInfo>,
}

#[derive(Debug, Clone, Copy)]
struct BindingInfo {
    /// Backing storage. Foundation uses register-only locals for
    /// non-captured names and an own-upvalue cell for names some
    /// inner function references (see [`capture`]).
    storage: BindingStorage,
    /// `true` for `const` declarations.
    is_const: bool,
    /// Whether the binding has been definitely initialized at the
    /// current compile point. `let x;` and `let x = init` start at
    /// `false` and flip to `true` after the initializer's
    /// `StoreLocal` / `StoreUpvalue`. Reads before that emit
    /// `Op::TdzError`.
    initialized: bool,
}

/// Where a binding lives in the running frame.
#[derive(Debug, Clone, Copy)]
enum BindingStorage {
    /// Plain register. Read with `LoadLocal`, written with
    /// `StoreLocal`.
    Register { reg: u16 },
    /// Own-upvalue cell at index `idx` in `frame.upvalues`. Used
    /// for any binding some inner function captures. Read /
    /// written with `LoadUpvalue` / `StoreUpvalue`.
    Upvalue { idx: u16 },
}

impl BindingStorage {
    fn to_argument_storage(self) -> ArgumentBindingStorage {
        match self {
            Self::Register { reg } => ArgumentBindingStorage::Register { reg },
            Self::Upvalue { idx } => ArgumentBindingStorage::Upvalue { idx },
        }
    }
}

/// One pending control-flow target so `break` / `continue` can patch
/// their offsets at scope close.
///
/// Tracks both real loops (`for` / `while` / `do-while` / `for-of` /
/// `for-in`) and pseudo-loops (`switch` body — only `break` is
/// legal, `continue` skips switch frames per spec §13.10.1).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iteration-statements>
/// - <https://tc39.es/ecma262/#sec-switch-statement>
/// - <https://tc39.es/ecma262/#sec-labelled-statements>
#[derive(Debug, Default)]
struct LoopFrame {
    /// Instruction PCs where `continue` emitted a placeholder
    /// JUMP. Patched to point at the loop's continue target (the
    /// update / test).
    continue_patches: Vec<u32>,
    /// Instruction PCs where `break` emitted a placeholder JUMP.
    /// Patched to point at the instruction after the loop body.
    break_patches: Vec<u32>,
    /// Optional label attached to this frame by an enclosing
    /// `LabeledStatement`. `break label;` matches against this
    /// field walking outward; `continue label;` only matches
    /// when [`LoopFrame::is_real_loop`] is true.
    label: Option<String>,
    /// `true` when the frame represents an iteration statement.
    /// `false` for a `switch` body, where `continue` must skip the
    /// frame and target the enclosing loop instead.
    is_real_loop: bool,
}

impl LoopFrame {
    fn iteration() -> Self {
        Self {
            continue_patches: Vec::new(),
            break_patches: Vec::new(),
            label: None,
            is_real_loop: true,
        }
    }

    fn switch_body() -> Self {
        Self {
            continue_patches: Vec::new(),
            break_patches: Vec::new(),
            label: None,
            is_real_loop: false,
        }
    }
}

/// Per-function compilation context.
#[derive(Debug)]
struct FunctionContext {
    module: Rc<RefCell<ModuleBuilder>>,
    code: Vec<Instruction>,
    spans: Vec<SpanEntry>,
    next_pc: u32,
    scratch: u16,
    /// Stack of lexical scopes. Index 0 is the function-body
    /// scope.
    scopes: Vec<Scope>,
    /// ECMAScript strictness for the function currently being
    /// lowered. This is compile-time metadata stored on the
    /// resulting bytecode function and also drives early errors.
    is_strict: bool,
    /// Stack of enclosing loops; the innermost is on top.
    loops: Vec<LoopFrame>,
    /// Label deposited by the immediately-enclosing
    /// `LabeledStatement` waiting to be consumed by the next pushed
    /// loop / switch frame. See [`compile_labeled_statement`].
    pending_label: Option<String>,
    /// Names that the entry-point pre-pass already compiled +
    /// stored as hoisted function declarations. The
    /// `Statement::FunctionDeclaration` arm checks this set and
    /// skips the source-position emission so the function isn't
    /// recompiled and its closure isn't re-stored.
    /// <https://tc39.es/ecma262/#sec-functiondeclarationinstantiation>
    hoisted_function_names: HashSet<String>,
    /// Names of this function's own bindings that some nested
    /// function references — populated by
    /// [`capture::analyze_function`] before code gen starts. Each
    /// such binding is allocated as an
    /// [`UpvalueCell`](otter_vm::UpvalueCell) instead of a register.
    captured_names: HashSet<String>,
    /// Simple formal names that must live in own-upvalue cells so a
    /// sloppy mapped arguments object can alias them without exposing
    /// frame registers outside the VM.
    mapped_argument_names: HashSet<String>,
    /// Number of own-upvalue cells allocated so far. The first
    /// `own_upvalue_count` slots in `frame.upvalues` belong to this
    /// function's own captured bindings.
    own_upvalue_count: u16,
    /// One entry per capture from the enclosing function. Each
    /// value is an absolute index into the **enclosing** frame's
    /// `upvalues` array — used as the source operand of
    /// `MakeClosure` when the parent emits the closure value.
    parent_captures: Vec<u32>,
    /// Map from captured-name → upvalue index in **this** function's
    /// `frame.upvalues`. Captures live at
    /// `own_upvalue_count..own_upvalue_count + parent_captures.len()`.
    captured_uv: HashMap<String, u16>,
    /// `Some` when this context is the top-level `<module-init>`
    /// of an ES-module fragment. Drives the lowering of
    /// `import` / `export` declarations + `import.meta` references
    /// against captured `module_env` / `import_meta` upvalues.
    /// Inner functions inherit module-mode lookups via the
    /// existing capture walk — they never set this themselves.
    module_state: Option<ModuleState>,
}

/// Compile-time stack of function contexts. The innermost context
/// is at the top; capture resolution walks this stack downward to
/// find a binding declared by an ancestor.
///
/// The compiler exposes the inner-most [`FunctionContext`] through
/// `Deref` / `DerefMut` so existing code continues to use `cx.emit`,
/// `cx.scratch`, etc. without referencing the stack explicitly.
#[derive(Debug)]
struct Compiler {
    stack: Vec<FunctionContext>,
    /// Stack of private-field namespace ids — one per enclosing
    /// class declaration. The top entry is the namespace used to
    /// mangle every `#name` reference inside the current class
    /// body. Empty when no class encloses the current expression
    /// (in which case `#name` references are a syntax error).
    /// Each entry is the integer suffix of `__priv_<n>_<name>`
    /// so peers across classes never collide.
    /// <https://tc39.es/ecma262/#sec-private-names>
    private_namespaces: Vec<u32>,
}

impl Compiler {
    fn new(top: FunctionContext) -> Self {
        Self {
            stack: vec![top],
            private_namespaces: Vec::new(),
        }
    }

    fn current_private_namespace(&self) -> Option<u32> {
        self.private_namespaces.last().copied()
    }

    fn mangle_private(&self, name: &str) -> Option<String> {
        self.current_private_namespace()
            .map(|ns| format!("__priv_{ns}_{name}"))
    }

    fn top_mut(&mut self) -> &mut FunctionContext {
        self.stack
            .last_mut()
            .expect("compiler context stack is empty")
    }

    fn push(&mut self, ctx: FunctionContext) {
        self.stack.push(ctx);
    }

    fn pop(&mut self) -> FunctionContext {
        self.stack
            .pop()
            .expect("compiler pop on empty context stack")
    }

    /// Walk the ancestor chain (excluding the top frame) and resolve
    /// `name` to an absolute upvalue index in the **top** frame's
    /// `frame.upvalues`. Each intermediate ancestor that didn't yet
    /// capture `name` gets a fresh capture slot pointing at the next
    /// ancestor up.
    fn resolve_capture(&mut self, name: &str) -> Option<u16> {
        if self.stack.len() < 2 {
            return None;
        }
        let top_idx = self.stack.len() - 1;
        // Already captured at top?
        if let Some(&idx) = self.stack[top_idx].captured_uv.get(name) {
            return Some(idx);
        }
        // Find the deepest ancestor that has `name` as an
        // own-upvalue (or already-resolved capture). Search from
        // direct-parent (top_idx - 1) downward.
        let mut found: Option<(usize, u16)> = None;
        for i in (0..top_idx).rev() {
            // Already-captured upvalue in this ancestor?
            if let Some(&idx) = self.stack[i].captured_uv.get(name) {
                found = Some((i, idx));
                break;
            }
            // Local binding declared as own-upvalue?
            let mut hit: Option<u16> = None;
            for scope in self.stack[i].scopes.iter().rev() {
                if let Some(info) = scope.bindings.get(name) {
                    if let BindingStorage::Upvalue { idx } = info.storage {
                        hit = Some(idx);
                    }
                    break;
                }
            }
            if let Some(idx) = hit {
                found = Some((i, idx));
                break;
            }
        }
        let (anchor_idx, mut current) = found?;
        // Cascade the cell from anchor down to the top frame: each
        // intermediate ancestor adds a capture entry pointing at the
        // previous one.
        for j in (anchor_idx + 1)..=top_idx {
            let frame = &mut self.stack[j];
            if let Some(&existing) = frame.captured_uv.get(name) {
                current = existing;
                continue;
            }
            let new_idx = frame
                .own_upvalue_count
                .checked_add(frame.parent_captures.len() as u16)
                .expect("captured upvalue index overflow");
            frame.parent_captures.push(current as u32);
            frame.captured_uv.insert(name.to_string(), new_idx);
            current = new_idx;
        }
        Some(current)
    }
}

impl std::ops::Deref for Compiler {
    type Target = FunctionContext;
    fn deref(&self) -> &Self::Target {
        self.stack.last().expect("compiler context stack is empty")
    }
}

impl std::ops::DerefMut for Compiler {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.stack
            .last_mut()
            .expect("compiler context stack is empty")
    }
}

impl FunctionContext {
    fn new(module: Rc<RefCell<ModuleBuilder>>) -> Self {
        Self {
            module,
            code: Vec::new(),
            spans: Vec::new(),
            next_pc: 0,
            scratch: 0,
            scopes: Vec::new(),
            is_strict: false,
            loops: Vec::new(),
            pending_label: None,
            hoisted_function_names: HashSet::new(),
            captured_names: HashSet::new(),
            mapped_argument_names: HashSet::new(),
            own_upvalue_count: 0,
            parent_captures: Vec::new(),
            captured_uv: HashMap::new(),
            module_state: None,
        }
    }

    fn with_strict(mut self, is_strict: bool) -> Self {
        self.is_strict = is_strict;
        self
    }

    /// Check `name` against this function's `captured_names` set
    /// (computed by the pre-pass) and, when present, allocate a
    /// fresh own-upvalue index for it. Returns the assigned index
    /// or `None` if the name is not captured (use a register
    /// instead).
    fn allocate_own_upvalue(&mut self, name: &str) -> Option<u16> {
        if !self.captured_names.contains(name) && !self.mapped_argument_names.contains(name) {
            return None;
        }
        let idx = self.own_upvalue_count;
        self.own_upvalue_count = idx.checked_add(1).expect("own_upvalue_count overflow");
        Some(idx)
    }

    fn alloc_scratch(&mut self) -> u16 {
        let r = self.scratch;
        self.scratch = self.scratch.checked_add(1).expect("register overflow");
        r
    }

    /// Push `frame` onto the loop stack, consuming any pending
    /// `LabeledStatement` label so `break label;` / `continue label;`
    /// inside the body resolves to this frame.
    fn push_loop_frame(&mut self, mut frame: LoopFrame) {
        frame.label = self.pending_label.take();
        self.loops.push(frame);
    }

    fn enter_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    /// Declare a synthetic binding whose storage is **always** an
    /// own-upvalue cell, regardless of whether the capture pre-pass
    /// flagged the name. Used by class lowering to set up
    /// `__class_home` and `__class_super` slots that inner methods
    /// resolve through the standard `resolve_capture` walk.
    fn declare_captured_binding(
        &mut self,
        name: &str,
        is_const: bool,
        span: (u32, u32),
    ) -> Result<BindingStorage, CompileError> {
        if self
            .scopes
            .last()
            .expect("declare_captured_binding called outside any scope")
            .bindings
            .contains_key(name)
        {
            return Err(CompileError::Unsupported {
                node: format!("redeclaration of `{name}` in same scope"),
                span,
            });
        }
        let idx = self.own_upvalue_count;
        self.own_upvalue_count = idx.checked_add(1).expect("own_upvalue_count overflow");
        let storage = BindingStorage::Upvalue { idx };
        let scope = self
            .scopes
            .last_mut()
            .expect("declare_captured_binding called outside any scope");
        scope.bindings.insert(
            name.to_string(),
            BindingInfo {
                storage,
                is_const,
                initialized: false,
            },
        );
        Ok(storage)
    }

    fn declare_binding(
        &mut self,
        name: &str,
        is_const: bool,
        span: (u32, u32),
    ) -> Result<BindingStorage, CompileError> {
        if self
            .scopes
            .last()
            .expect("declare_binding called outside any scope")
            .bindings
            .contains_key(name)
        {
            return Err(CompileError::Unsupported {
                node: format!("redeclaration of `{name}` in same scope"),
                span,
            });
        }
        let storage = if let Some(idx) = self.allocate_own_upvalue(name) {
            BindingStorage::Upvalue { idx }
        } else {
            let reg = self.scratch;
            self.scratch = self.scratch.checked_add(1).expect("register overflow");
            BindingStorage::Register { reg }
        };
        let scope = self
            .scopes
            .last_mut()
            .expect("declare_binding called outside any scope");
        scope.bindings.insert(
            name.to_string(),
            BindingInfo {
                storage,
                is_const,
                initialized: false,
            },
        );
        Ok(storage)
    }

    fn lookup_binding(&self, name: &str) -> Option<BindingInfo> {
        for scope in self.scopes.iter().rev() {
            if let Some(info) = scope.bindings.get(name) {
                return Some(*info);
            }
        }
        None
    }

    /// Look up `name` only in the *innermost* scope. Used by the
    /// `let` / `const` arm to detect bindings the lexical pre-pass
    /// already created at the function / script / module top level.
    fn lookup_in_current_scope(&self, name: &str) -> Option<BindingInfo> {
        self.scopes
            .last()
            .and_then(|scope| scope.bindings.get(name).copied())
    }

    /// Flip a binding's `initialized` flag to `true` once we've
    /// emitted its initializer's store. The compiler is intentionally
    /// conservative: we never flip back to `false` and we never
    /// "merge" branch states — task 14 ships the simple definite-
    /// assignment rule and leaves branch-aware refinement for a
    /// future slice.
    fn mark_initialized(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.bindings.get_mut(name) {
                info.initialized = true;
                return;
            }
        }
    }

    /// Emit an [`Op::EnterTry`] with placeholder catch / finally
    /// offsets and an exception register. Returns the instruction
    /// pc so the caller can patch the targeted offset to the
    /// emitted catch / finally landing.
    ///
    /// `catch_offset` and `finally_offset` are the **initial**
    /// values stored in the operand list — typically a real
    /// `0` placeholder for whichever clause needs patching, and
    /// [`otter_vm::NO_HANDLER_OFFSET`] for the absent clause.
    fn emit_enter_try(
        &mut self,
        catch_offset: i32,
        finally_offset: i32,
        exc_reg: u16,
        span: (u32, u32),
    ) -> u32 {
        let pc = self.next_pc;
        self.code.push(Instruction {
            pc,
            op: Op::EnterTry,
            operands: vec![
                Operand::Imm32(catch_offset),
                Operand::Imm32(finally_offset),
                Operand::Register(exc_reg),
            ],
        });
        self.spans.push(SpanEntry { pc, span });
        self.next_pc += 1;
        pc
    }

    /// Patch a previously emitted [`Op::EnterTry`] so that one of
    /// its offsets targets the **current** `next_pc`. Pass `true`
    /// for `is_catch` to patch the catch offset, `false` to patch
    /// the finally offset. The non-targeted offset is left
    /// untouched (kept as the `NO_HANDLER_OFFSET` sentinel the
    /// initial emit installed).
    fn patch_enter_try_offset(&mut self, enter_pc: u32, is_catch: bool) {
        let target = self.next_pc;
        let offset = target as i64 - (enter_pc as i64 + 1);
        let offset = i32::try_from(offset).expect("EnterTry offset out of i32 range");
        let instr = self
            .code
            .iter_mut()
            .find(|i| i.pc == enter_pc)
            .expect("patch target missing");
        debug_assert!(matches!(instr.op, Op::EnterTry));
        let slot_idx = if is_catch { 0 } else { 1 };
        match instr.operands.get_mut(slot_idx) {
            Some(Operand::Imm32(slot)) => *slot = offset,
            _ => panic!("EnterTry operand at index {slot_idx} not Imm32"),
        }
    }

    /// Emit a placeholder branch and return its instruction index
    /// so a later [`Self::patch_branch`] can fill in the offset.
    fn emit_branch_placeholder(&mut self, op: Op, cond_reg: Option<u16>, span: (u32, u32)) -> u32 {
        let mut operands: Vec<Operand> = Vec::with_capacity(2);
        operands.push(Operand::Imm32(0));
        if let Some(reg) = cond_reg {
            operands.push(Operand::Register(reg));
        }
        let pc = self.next_pc;
        self.code.push(Instruction { pc, op, operands });
        self.spans.push(SpanEntry { pc, span });
        self.next_pc += 1;
        pc
    }

    /// Patch a previously emitted branch so it targets the
    /// **current** `next_pc`.
    fn patch_branch_to_here(&mut self, branch_pc: u32) {
        let target = self.next_pc;
        self.patch_branch(branch_pc, target);
    }

    /// Patch a previously emitted branch to point at `target_pc`.
    fn patch_branch(&mut self, branch_pc: u32, target_pc: u32) {
        let offset = target_pc as i64 - (branch_pc as i64 + 1);
        let offset = i32::try_from(offset).expect("branch offset out of i32 range");
        let instr = self
            .code
            .iter_mut()
            .find(|i| i.pc == branch_pc)
            .expect("patch target missing");
        if let Some(Operand::Imm32(slot)) = instr.operands.first_mut() {
            *slot = offset;
        } else {
            panic!("patch target operand not Imm32");
        }
    }

    fn intern_string_constant(&mut self, value: &str) -> u32 {
        let utf16: Vec<u16> = value.encode_utf16().collect();
        self.intern_utf16_string_constant(utf16)
    }

    /// Intern a pre-built WTF-16 unit vector. Used for string
    /// literals that carry lone surrogates: oxc encodes those via
    /// the §11.8.4 [`StringLiteral`](https://tc39.es/ecma262/#sec-literals-string-literals)
    /// lossy scheme (`\u{FFFD}XXXX` per lone surrogate, `\u{FFFD}fffd`
    /// for a literal U+FFFD), so the compiler decodes it into the
    /// original code-unit sequence before interning.
    fn intern_utf16_string_constant(&mut self, utf16: Vec<u16>) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::String { utf16: existing } = c
                && existing == &utf16
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::String { utf16 });
        (module.constants.len() - 1) as u32
    }

    fn intern_number_constant(&mut self, value: f64) -> u32 {
        let bits = value.to_bits();
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::Number { bits: existing } = c
                && *existing == bits
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::Number { bits });
        (module.constants.len() - 1) as u32
    }

    fn intern_bigint_constant(&mut self, decimal: &str) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::BigInt { decimal: existing } = c
                && existing == decimal
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::BigInt {
            decimal: decimal.to_string(),
        });
        (module.constants.len() - 1) as u32
    }

    fn intern_regexp_constant(&mut self, pattern_utf16: &[u16], flags: &str) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::RegExp {
                pattern_utf16: existing_pat,
                flags: existing_flags,
            } = c
                && existing_pat == pattern_utf16
                && existing_flags == flags
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::RegExp {
            pattern_utf16: pattern_utf16.to_vec(),
            flags: flags.to_string(),
        });
        (module.constants.len() - 1) as u32
    }

    fn intern_function_id(&mut self, function_id: u32) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::FunctionId { index } = c
                && *index == function_id
            {
                return i as u32;
            }
        }
        module
            .constants
            .push(Constant::FunctionId { index: function_id });
        (module.constants.len() - 1) as u32
    }

    fn emit(&mut self, op: Op, operands: Vec<Operand>, span: (u32, u32)) {
        let pc = self.next_pc;
        self.code.push(Instruction { pc, op, operands });
        self.spans.push(SpanEntry { pc, span });
        self.next_pc += 1;
    }

    /// Emit the appropriate "load this binding into `dst`" op pair
    /// for the binding's storage kind.
    fn emit_load_storage(&mut self, dst: u16, storage: BindingStorage, span: (u32, u32)) {
        match storage {
            BindingStorage::Register { reg } => self.emit(
                Op::LoadLocal,
                vec![Operand::Register(dst), Operand::Imm32(reg as i32)],
                span,
            ),
            BindingStorage::Upvalue { idx } => self.emit(
                Op::LoadUpvalue,
                vec![Operand::Register(dst), Operand::Imm32(idx as i32)],
                span,
            ),
        }
    }

    /// In module mode, mirror an assignment to an exported name
    /// through to the captured `module_env` JsObject so importers'
    /// later reads observe the new value (live bindings).
    /// No-op when `name` is not in the export set or when not in
    /// module mode.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-module-environment-records-setmutablebinding-n-v-s>
    fn emit_module_export_mirror(&mut self, name: &str, value_reg: u16, span: (u32, u32)) {
        let env_uv = match &self.module_state {
            Some(state) if state.exported_names.contains(name) => state.module_env_uv,
            _ => return,
        };
        let env_reg = self.alloc_scratch();
        self.emit(
            Op::LoadUpvalue,
            vec![Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
            span,
        );
        self.emit_store_property(env_reg, name, value_reg, span);
    }

    /// Mirror `value_reg` through to `module_env.default`. Used by
    /// `export default function f(){}` from the hoist pass: the
    /// default export entry was registered by the module pre-pass
    /// so the closure must land on `module_env.default` even when
    /// no source-position store ever runs (the source-position arm
    /// becomes a no-op for hoisted names).
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-exports-runtime-semantics-evaluation>
    fn emit_module_export_default_mirror(&mut self, value_reg: u16, span: (u32, u32)) {
        let env_uv = match &self.module_state {
            Some(state) => state.module_env_uv,
            None => return,
        };
        let env_reg = self.alloc_scratch();
        self.emit(
            Op::LoadUpvalue,
            vec![Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
            span,
        );
        self.emit_store_property(env_reg, "default", value_reg, span);
    }

    /// Emit `Op::StoreProperty obj_reg, name_const, src_reg, scratch`.
    /// Used by the module-mode lowering to mirror writes through
    /// to `module_env` for exported bindings, and by the export
    /// declaration arms. The `scratch` slot is reserved for
    /// accessor-setter dispatch per [`Op::StoreProperty`]'s contract.
    fn emit_store_property(&mut self, obj_reg: u16, name: &str, src: u16, span: (u32, u32)) {
        let name_const = self.intern_string_constant(name);
        let scratch = self.alloc_scratch();
        self.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_const),
                Operand::Register(src),
                Operand::Register(scratch),
            ],
            span,
        );
    }

    /// Emit `Op::StoreElement obj_reg, key_reg, src_reg, scratch`.
    /// The scratch slot is reserved for computed-property accessor
    /// setter dispatch and mirrors `Op::StoreProperty`.
    fn emit_store_element(&mut self, obj_reg: u16, key_reg: u16, src: u16, span: (u32, u32)) {
        let scratch = self.alloc_scratch();
        self.emit(
            Op::StoreElement,
            vec![
                Operand::Register(obj_reg),
                Operand::Register(key_reg),
                Operand::Register(src),
                Operand::Register(scratch),
            ],
            span,
        );
    }

    /// Emit `Op::LoadProperty dst, obj_reg, name_const`. Used by
    /// the module-mode lowering for imported-name reads
    /// (`LoadProperty import_record, "name"`) and `import.meta.url`.
    fn emit_load_property(&mut self, dst: u16, obj_reg: u16, name: &str, span: (u32, u32)) {
        let name_const = self.intern_string_constant(name);
        self.emit(
            Op::LoadProperty,
            vec![
                Operand::Register(dst),
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_const),
            ],
            span,
        );
    }

    /// Emit the "write `src` into this binding" op pair for the
    /// storage kind.
    fn emit_store_storage(&mut self, src: u16, storage: BindingStorage, span: (u32, u32)) {
        match storage {
            BindingStorage::Register { reg } => self.emit(
                Op::StoreLocal,
                vec![Operand::Register(src), Operand::Imm32(reg as i32)],
                span,
            ),
            BindingStorage::Upvalue { idx } => self.emit(
                Op::StoreUpvalue,
                vec![Operand::Register(src), Operand::Imm32(idx as i32)],
                span,
            ),
        }
    }
}

/// Compile one statement. Returns `Some(reg)` when the statement is
/// an `ExpressionStatement` whose value should propagate as the
/// program's completion value; `None` otherwise.
fn compile_statement(cx: &mut Compiler, stmt: &Statement<'_>) -> Result<Option<u16>, CompileError> {
    if is_erased_ts_statement(stmt) {
        return Ok(None);
    }
    if let Some((node, span)) = rejected_ts_statement(stmt) {
        return Err(CompileError::TypeScriptUnsupported {
            node: node.to_string(),
            span,
        });
    }
    match stmt {
        Statement::EmptyStatement(_) => Ok(None),

        Statement::ExpressionStatement(es) => {
            let span = (es.span.start, es.span.end);
            let reg = compile_expr(cx, &es.expression, span)?;
            Ok(Some(reg))
        }

        Statement::BlockStatement(b) => {
            let span = (b.span.start, b.span.end);
            cx.enter_scope();
            let mut last = None;
            for inner in &b.body {
                if let Some(r) = compile_statement(cx, inner)? {
                    last = Some(r);
                }
            }
            cx.exit_scope();
            let _ = span;
            Ok(last)
        }

        Statement::VariableDeclaration(decl) => {
            let is_const = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Const);
            let is_var = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
            // §14.3.2 VariableStatement — the binding itself was
            // hoisted to the enclosing function / script / module
            // variable scope by the entry-point pre-pass. Here we
            // only have to evaluate each initializer (when present)
            // and store it into the pre-bound storage.
            // <https://tc39.es/ecma262/#sec-variable-statement>
            if is_var {
                for declarator in &decl.declarations {
                    let span = (declarator.span.start, declarator.span.end);
                    if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                        let name = id.name.as_str().to_string();
                        // No initializer → leave the hoisted
                        // `undefined` in place per §14.3.2.1
                        // RuntimeSemantics: Evaluation of
                        // VariableDeclaration step 1.
                        let Some(init) = &declarator.init else {
                            continue;
                        };
                        let info = cx.lookup_binding(&name).ok_or(CompileError::Unsupported {
                            node: format!("var `{name}` not pre-hoisted"),
                            span,
                        })?;
                        let init_reg = compile_expr(cx, init, span)?;
                        cx.emit_store_storage(init_reg, info.storage, span);
                        cx.emit_module_export_mirror(&name, init_reg, span);
                        continue;
                    }
                    // Destructuring `var [a, b] = x`. Spec-correct
                    // semantics: every name was already hoisted —
                    // we walk the pattern and assign each component
                    // into the pre-existing var binding.
                    let init = declarator.init.as_ref().ok_or(CompileError::Unsupported {
                        node: "var destructuring requires an initializer".to_string(),
                        span,
                    })?;
                    let init_reg = compile_expr(cx, init, span)?;
                    destructure_assign(cx, init_reg, &declarator.id, span)?;
                }
                return Ok(None);
            }
            for declarator in &decl.declarations {
                let span = (declarator.span.start, declarator.span.end);
                // Fast path for the overwhelmingly common
                // `let x = init;` shape so the simple binding
                // doesn't pay an extra register copy.
                if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                    let name = id.name.as_str().to_string();
                    // The entry-point lexical pre-pass already
                    // declared every top-level `let` / `const` name
                    // (TDZ until source-position store) so inner
                    // function declarations could capture them.
                    // Reuse the existing binding when it's there;
                    // otherwise (nested block-scoped declaration)
                    // create a fresh one.
                    let storage = match cx.lookup_in_current_scope(&name) {
                        Some(info) => info.storage,
                        None => cx.declare_binding(&name, is_const, span)?,
                    };
                    let init_reg = match &declarator.init {
                        Some(init) => compile_expr(cx, init, span)?,
                        None => {
                            let dst = cx.alloc_scratch();
                            cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
                            dst
                        }
                    };
                    cx.emit_store_storage(init_reg, storage, span);
                    cx.mark_initialized(&name);
                    cx.emit_module_export_mirror(&name, init_reg, span);
                    continue;
                }
                // Destructuring binding (`let [a, b] = …` /
                // `let { x } = …`) — `var` bindings are rejected
                // earlier so we can route everything through the
                // shared destructuring helper.
                let init = declarator.init.as_ref().ok_or(CompileError::Unsupported {
                    node: "VariableDeclarator: destructuring requires an initializer".to_string(),
                    span,
                })?;
                let init_reg = compile_expr(cx, init, span)?;
                destructure_into(cx, init_reg, &declarator.id, span)?;
            }
            Ok(None)
        }

        Statement::IfStatement(s) => {
            let span = (s.span.start, s.span.end);
            let cond_reg = compile_expr(cx, &s.test, span)?;
            // JUMP_IF_FALSE → after consequent
            let jmp_if_false = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span);
            compile_statement(cx, &s.consequent)?;
            if let Some(alt) = &s.alternate {
                // After consequent, unconditional JUMP past the
                // alternate.
                let jmp_end = cx.emit_branch_placeholder(Op::Jump, None, span);
                cx.patch_branch_to_here(jmp_if_false);
                compile_statement(cx, alt)?;
                cx.patch_branch_to_here(jmp_end);
            } else {
                cx.patch_branch_to_here(jmp_if_false);
            }
            Ok(None)
        }

        Statement::WhileStatement(s) => {
            let span = (s.span.start, s.span.end);
            let loop_top = cx.next_pc;
            cx.push_loop_frame(LoopFrame::iteration());
            let cond_reg = compile_expr(cx, &s.test, span)?;
            let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span);
            compile_statement(cx, &s.body)?;
            // Back-edge jump to loop top.
            let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch(back_jmp, loop_top);
            cx.patch_branch_to_here(exit_jmp);
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, loop_top);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            Ok(None)
        }

        Statement::DoWhileStatement(s) => {
            let span = (s.span.start, s.span.end);
            let body_top = cx.next_pc;
            cx.push_loop_frame(LoopFrame::iteration());
            compile_statement(cx, &s.body)?;
            let continue_target = cx.next_pc;
            let cond_reg = compile_expr(cx, &s.test, span)?;
            let back_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(cond_reg), span);
            cx.patch_branch(back_jmp, body_top);
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, continue_target);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            Ok(None)
        }

        Statement::ForStatement(s) => {
            let span = (s.span.start, s.span.end);
            cx.enter_scope();
            // Initializer.
            if let Some(init) = &s.init {
                match init {
                    oxc_ast::ast::ForStatementInit::VariableDeclaration(decl) => {
                        compile_for_init_decl(cx, decl, span)?;
                    }
                    other => {
                        if let Some(expr) = init_to_expression(other) {
                            compile_expr(cx, expr, span)?;
                        }
                    }
                }
            }
            cx.push_loop_frame(LoopFrame::iteration());
            let test_top = cx.next_pc;
            // Test.
            let exit_patch = if let Some(test) = &s.test {
                let cond_reg = compile_expr(cx, test, span)?;
                Some(cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span))
            } else {
                None
            };
            // Body.
            compile_statement(cx, &s.body)?;
            // Continue lands on the update.
            let update_pc = cx.next_pc;
            if let Some(update) = &s.update {
                compile_expr(cx, update, span)?;
            }
            let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch(back_jmp, test_top);
            if let Some(p) = exit_patch {
                cx.patch_branch_to_here(p);
            }
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, update_pc);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            cx.exit_scope();
            Ok(None)
        }

        Statement::ForOfStatement(s) => compile_for_of_statement(cx, s),

        Statement::BreakStatement(s) => {
            let span = (s.span.start, s.span.end);
            // §14.13 / §13.15: `break label;` targets the matching
            // labelled enclosing statement (loop or switch). Bare
            // `break;` targets the innermost loop or switch frame.
            // <https://tc39.es/ecma262/#sec-break-statement>
            let label = s.label.as_ref().map(|id| id.name.as_str().to_string());
            let target_idx = match &label {
                None => cx
                    .loops
                    .len()
                    .checked_sub(1)
                    .ok_or(CompileError::Unsupported {
                        node: "BreakStatement outside any loop or switch".to_string(),
                        span,
                    })?,
                Some(name) => cx
                    .loops
                    .iter()
                    .rposition(|f| f.label.as_deref() == Some(name.as_str()))
                    .ok_or_else(|| CompileError::Unsupported {
                        node: format!("BreakStatement: unknown label `{name}`"),
                        span,
                    })?,
            };
            let pc = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.loops[target_idx].break_patches.push(pc);
            Ok(None)
        }

        Statement::FunctionDeclaration(f) => {
            let span = (f.span.start, f.span.end);
            let name =
                f.id.as_ref()
                    .ok_or(CompileError::Unsupported {
                        node: "FunctionDeclaration without name".to_string(),
                        span,
                    })?
                    .name
                    .as_str()
                    .to_string();
            // §10.2.11 step 30 — top-level function declarations
            // were hoisted at scope entry by
            // `hoist_function_declarations`. Skip the source-position
            // re-emit when the name was hoisted; otherwise (nested
            // block declaration in strict mode) fall through to the
            // ordinary block-scoped binding path.
            if cx.hoisted_function_names.contains(&name) {
                return Ok(None);
            }
            let (function_id, captures) = compile_function_full(
                cx,
                &name,
                &f.params,
                &f.body,
                span,
                f.r#async,
                f.generator,
                false,
            )?;
            let storage = cx.declare_binding(&name, false, span)?;
            let const_idx = cx.intern_function_id(function_id);
            let tmp = cx.alloc_scratch();
            emit_make_callable(cx, tmp, const_idx, &captures, false, span);
            cx.emit_store_storage(tmp, storage, span);
            cx.mark_initialized(&name);
            cx.emit_module_export_mirror(&name, tmp, span);
            Ok(None)
        }

        Statement::ReturnStatement(r) => {
            let span = (r.span.start, r.span.end);
            match &r.argument {
                Some(arg) => {
                    let reg = compile_expr(cx, arg, span)?;
                    cx.emit(Op::ReturnValue, vec![Operand::Register(reg)], span);
                }
                None => {
                    cx.emit(Op::ReturnUndefined, vec![], span);
                }
            }
            Ok(None)
        }

        Statement::ClassDeclaration(class) => {
            let span = (class.span.start, class.span.end);
            let name = class
                .id
                .as_ref()
                .ok_or(CompileError::Unsupported {
                    node: "ClassDeclaration without name".to_string(),
                    span,
                })?
                .name
                .as_str()
                .to_string();
            let class_reg = compile_class(cx, class, Some(&name))?;
            // The lexical pre-pass may have already declared this
            // class name (TDZ) so inner functions could capture it.
            let storage = match cx.lookup_in_current_scope(&name) {
                Some(info) => info.storage,
                None => cx.declare_binding(&name, false, span)?,
            };
            cx.emit_store_storage(class_reg, storage, span);
            cx.mark_initialized(&name);
            cx.emit_module_export_mirror(&name, class_reg, span);
            Ok(None)
        }

        Statement::ThrowStatement(s) => {
            let span = (s.span.start, s.span.end);
            let reg = compile_expr(cx, &s.argument, span)?;
            cx.emit(Op::Throw, vec![Operand::Register(reg)], span);
            Ok(None)
        }

        Statement::TryStatement(s) => compile_try_statement(cx, s),

        Statement::ImportDeclaration(decl) => {
            // Type-only `import type { … }` is erased earlier via
            // `is_erased_ts_statement`. Runtime imports were
            // pre-resolved by `compile_module_fragment`'s pre-pass:
            // the import-record upvalue is already populated and
            // identifier resolution routes references through
            // `imported_names`. Nothing left to do at the
            // statement site.
            //
            // Outside module mode, an import is a hard error: the
            // foundation rejects mixed script + module input.
            //
            // Spec: <https://tc39.es/ecma262/#sec-imports>
            let span = (decl.span.start, decl.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "ImportDeclaration outside ES-module fragment".to_string(),
                    span,
                });
            }
            Ok(None)
        }

        Statement::ExportNamedDeclaration(decl) => {
            // Three shapes:
            //   1. `export let x = …` / `export function f() {…}` /
            //      `export class C {…}` — the inner declaration
            //      compiles via the regular path and the export
            //      mirror runs because the pre-pass added the name
            //      to `exported_names`.
            //   2. `export { a, b as c }` — re-export of names that
            //      already live in the module body. We synthesise a
            //      property store on `module_env` for each spec.
            //   3. `export { x } from "./other.ts"` (re-export from
            //      another module) — read from the source's import
            //      record, write to `module_env`.
            //
            // Spec: <https://tc39.es/ecma262/#sec-exports>
            let span = (decl.span.start, decl.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "ExportNamedDeclaration outside ES-module fragment".to_string(),
                    span,
                });
            }
            if let Some(inner) = &decl.declaration {
                compile_export_inner_declaration(cx, inner, span)?;
            }
            // Mirror each `export { name as exported_name }` spec
            // through to module_env. When `decl.source` is set the
            // source is another module; we read from its import
            // record (already present courtesy of the pre-pass).
            let from_source = decl.source.as_ref().map(|s| s.value.as_str().to_string());
            for spec in &decl.specifiers {
                let exported = module_export_name_to_str(&spec.exported);
                let local = module_export_name_to_str(&spec.local);
                let value_reg = if let Some(src) = &from_source {
                    let record_uv = cx
                        .module_state
                        .as_ref()
                        .and_then(|s| s.import_records.get(src).copied())
                        .ok_or(CompileError::Unsupported {
                            node: format!(
                                "ExportNamedDeclaration: unresolved re-export source `{src}`"
                            ),
                            span,
                        })?;
                    let record_reg = cx.alloc_scratch();
                    cx.emit(
                        Op::LoadUpvalue,
                        vec![
                            Operand::Register(record_reg),
                            Operand::Imm32(record_uv as i32),
                        ],
                        span,
                    );
                    let dst = cx.alloc_scratch();
                    cx.emit_load_property(dst, record_reg, &local, span);
                    dst
                } else if decl.declaration.is_some() {
                    // The declaration arm already mirrored via
                    // emit_module_export_mirror. Nothing to do.
                    continue;
                } else {
                    // `export { name }` — read from the local
                    // binding (the body must have declared it).
                    let info = cx.lookup_binding(&local).ok_or(CompileError::Unsupported {
                        node: format!("export of undeclared `{local}`"),
                        span,
                    })?;
                    let dst = cx.alloc_scratch();
                    cx.emit_load_storage(dst, info.storage, span);
                    dst
                };
                let env_uv = cx
                    .module_state
                    .as_ref()
                    .map(|s| s.module_env_uv)
                    .expect("module_state checked above");
                let env_reg = cx.alloc_scratch();
                cx.emit(
                    Op::LoadUpvalue,
                    vec![Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
                    span,
                );
                cx.emit_store_property(env_reg, &exported, value_reg, span);
            }
            Ok(None)
        }

        Statement::ExportDefaultDeclaration(decl) => {
            // `export default expr` — evaluate the expression
            // then store on `module_env.default`.
            //
            // Spec: <https://tc39.es/ecma262/#sec-exports-runtime-semantics-evaluation>
            let span = (decl.span.start, decl.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "ExportDefaultDeclaration outside ES-module fragment".to_string(),
                    span,
                });
            }
            let value_reg = match &decl.declaration {
                oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    // §15.2 — a named `export default function f(){}`
                    // is a HoistableDeclaration: the closure was
                    // already compiled and bound to `f` (and
                    // mirrored as `module_env.default`) by
                    // [`hoist_function_declarations`]. The source-
                    // position arm becomes a pure no-op so the
                    // hoist's instructions stay the single source
                    // of truth.
                    let hoisted_name =
                        f.id.as_ref()
                            .map(|id| id.name.as_str().to_string())
                            .filter(|name| cx.hoisted_function_names.contains(name));
                    if hoisted_name.is_some() {
                        return Ok(None);
                    }
                    let name =
                        f.id.as_ref()
                            .map(|id| id.name.as_str().to_string())
                            .unwrap_or_else(|| "default".to_string());
                    let (function_id, captures) = compile_function_full(
                        cx,
                        &name,
                        &f.params,
                        &f.body,
                        span,
                        f.r#async,
                        f.generator,
                        false,
                    )?;
                    let const_idx = cx.intern_function_id(function_id);
                    let dst = cx.alloc_scratch();
                    emit_make_callable(cx, dst, const_idx, &captures, false, span);
                    dst
                }
                oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                    let name = c.id.as_ref().map(|id| id.name.as_str().to_string());
                    compile_class(cx, c, name.as_deref())?
                }
                other => {
                    let expr = other.as_expression().ok_or(CompileError::Unsupported {
                        node: "ExportDefaultDeclaration: unsupported declaration kind".to_string(),
                        span,
                    })?;
                    compile_expr(cx, expr, span)?
                }
            };
            let env_uv = cx
                .module_state
                .as_ref()
                .map(|s| s.module_env_uv)
                .expect("module_state checked above");
            let env_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                vec![Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
                span,
            );
            cx.emit_store_property(env_reg, "default", value_reg, span);
            Ok(None)
        }

        Statement::ExportAllDeclaration(decl) => {
            // `export * from "./other.ts"` and `export * as ns from
            // "./other.ts"`. Foundation supports the latter as a
            // simple re-export (the source module's namespace
            // becomes a property on module_env). The bare
            // `export *` would need to copy every own property of
            // the source's namespace into our module_env at the
            // moment of evaluation — doable but would also need
            // re-resolution of those names on every read for live
            // bindings, which the foundation does not yet model.
            // Reject the bare form for now.
            //
            // Spec: <https://tc39.es/ecma262/#sec-getmoduleexports>
            let span = (decl.span.start, decl.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "ExportAllDeclaration outside ES-module fragment".to_string(),
                    span,
                });
            }
            let source = decl.source.value.as_str().to_string();
            let record_uv = cx
                .module_state
                .as_ref()
                .and_then(|s| s.import_records.get(&source).copied())
                .ok_or(CompileError::Unsupported {
                    node: format!("ExportAllDeclaration: unresolved source `{source}`"),
                    span,
                })?;
            let exported_alias = decl
                .exported
                .as_ref()
                .map(module_export_name_to_str)
                .ok_or(CompileError::Unsupported {
                    node: "ExportAllDeclaration: bare `export *` not yet supported".to_string(),
                    span,
                })?;
            let record_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                vec![
                    Operand::Register(record_reg),
                    Operand::Imm32(record_uv as i32),
                ],
                span,
            );
            let env_uv = cx
                .module_state
                .as_ref()
                .map(|s| s.module_env_uv)
                .expect("module_state checked above");
            let env_reg = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                vec![Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
                span,
            );
            cx.emit_store_property(env_reg, &exported_alias, record_reg, span);
            Ok(None)
        }

        Statement::ContinueStatement(s) => {
            let span = (s.span.start, s.span.end);
            // §14.8: `continue` targets the innermost real iteration
            // statement (switch frames are skipped per §13.10.1).
            // `continue label;` targets the matching labelled loop.
            // <https://tc39.es/ecma262/#sec-continue-statement>
            let label = s.label.as_ref().map(|id| id.name.as_str().to_string());
            let target_idx = match &label {
                None => cx.loops.iter().rposition(|f| f.is_real_loop).ok_or(
                    CompileError::Unsupported {
                        node: "ContinueStatement outside any loop".to_string(),
                        span,
                    },
                )?,
                Some(name) => {
                    let idx = cx
                        .loops
                        .iter()
                        .rposition(|f| f.label.as_deref() == Some(name.as_str()))
                        .ok_or_else(|| CompileError::Unsupported {
                            node: format!("ContinueStatement: unknown label `{name}`"),
                            span,
                        })?;
                    if !cx.loops[idx].is_real_loop {
                        return Err(CompileError::Unsupported {
                            node: format!("ContinueStatement: label `{name}` does not name a loop"),
                            span,
                        });
                    }
                    idx
                }
            };
            let pc = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.loops[target_idx].continue_patches.push(pc);
            Ok(None)
        }

        // §14.13 — `with` is forbidden in strict mode and ES modules.
        // Foundation is always strict, so reject with a clear
        // diagnostic rather than the generic "unsupported".
        // <https://tc39.es/ecma262/#sec-with-statement>
        Statement::WithStatement(w) => Err(CompileError::Unsupported {
            node: "WithStatement is forbidden in strict mode / ES modules (§14.13)".to_string(),
            span: (w.span.start, w.span.end),
        }),

        Statement::SwitchStatement(s) => compile_switch_statement(cx, s),

        Statement::ForInStatement(s) => compile_for_in_statement(cx, s),

        Statement::LabeledStatement(s) => compile_labeled_statement(cx, s),

        other => Err(CompileError::Unsupported {
            node: stmt_kind_name(other).to_string(),
            span: stmt_span(other),
        }),
    }
}

/// Helper for the `for(...; ...; ...)` initializer's
/// `let`/`const`/`var` declaration form. Mirrors the
/// `VariableDeclaration` arm of `compile_statement` but operates on
/// the borrowed declaration without re-cloning it through OXC's
/// allocator.
fn compile_for_init_decl(
    cx: &mut Compiler,
    decl: &oxc_ast::ast::VariableDeclaration<'_>,
    _span: (u32, u32),
) -> Result<(), CompileError> {
    let is_const = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Const);
    let is_var = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
    for declarator in &decl.declarations {
        let span = (declarator.span.start, declarator.span.end);
        match &declarator.id {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) => {
                let name = id.name.as_str().to_string();
                // §14.7.4 ForLoopEvaluation — `var` re-uses the
                // function-scope binding pre-hoisted at function
                // entry; `let`/`const` declare a fresh per-loop
                // binding.
                let storage = if is_var {
                    cx.lookup_binding(&name)
                        .ok_or(CompileError::Unsupported {
                            node: format!("for-init var `{name}` not pre-hoisted"),
                            span,
                        })?
                        .storage
                } else {
                    cx.declare_binding(&name, is_const, span)?
                };
                let init_reg = match &declarator.init {
                    Some(init) => compile_expr(cx, init, span)?,
                    None => {
                        let dst = cx.alloc_scratch();
                        cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
                        dst
                    }
                };
                cx.emit_store_storage(init_reg, storage, span);
                cx.mark_initialized(&name);
            }
            // §14.7.4 — destructuring init: `for (const [a, b] = …; …)`.
            // The initializer is required (let/const/var destructuring
            // without an initializer is a SyntaxError; oxc enforces
            // that).
            _ => {
                let init = declarator.init.as_ref().ok_or(CompileError::Unsupported {
                    node: "for-init destructuring without initializer".to_string(),
                    span,
                })?;
                let init_reg = compile_expr(cx, init, span)?;
                if is_var {
                    destructure_assign(cx, init_reg, &declarator.id, span)?;
                } else {
                    destructure_into(cx, init_reg, &declarator.id, span)?;
                }
            }
        }
    }
    Ok(())
}

/// Compile a function body into a fresh `Function` record and
/// return its id together with the captures it inherits from
/// `parent`. Parameters live in registers `0..param_count` (the
/// raw incoming argv slots); each one is then destructured /
/// defaulted / aliased into named bindings as the body expects.
/// Rest parameters (`...t`) are materialised by the runtime via
/// [`Op::CollectRest`] reading from the call frame's stashed
/// trailing argument list.
/// `true` when any expression at module-top-level (i.e. outside
/// any function / class body) uses `await`. The compiler upgrades
/// `<main>` / `<module-init>` to `is_async = true` when this
/// returns true so `Op::Await` can park the entry frame, matching
/// §16.2.1.7 `top-level-await` modules.
/// Walk a non-arrow function body and report whether it references
/// the `arguments` identifier in a binding that escapes the body's
/// own arrow / nested-function scopes. Arrow functions inherit
/// `arguments` lexically per §10.2.1.4 so a reference inside an
/// arrow within the body still implies the enclosing function
/// must materialise the object.
fn body_references_arguments(
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: Option<&oxc_ast::ast::FunctionBody<'_>>,
) -> bool {
    use oxc_ast_visit::Visit;
    #[derive(Default)]
    struct ArgsFinder {
        nested_function_depth: u32,
        found: bool,
    }
    impl<'a> Visit<'a> for ArgsFinder {
        fn visit_function(
            &mut self,
            it: &oxc_ast::ast::Function<'a>,
            flags: oxc_syntax::scope::ScopeFlags,
        ) {
            // Nested non-arrow function — has its own `arguments`.
            self.nested_function_depth += 1;
            oxc_ast_visit::walk::walk_function(self, it, flags);
            self.nested_function_depth -= 1;
        }
        fn visit_class_body(&mut self, it: &oxc_ast::ast::ClassBody<'a>) {
            // Class methods are functions with their own arguments.
            self.nested_function_depth += 1;
            oxc_ast_visit::walk::walk_class_body(self, it);
            self.nested_function_depth -= 1;
        }
        fn visit_identifier_reference(&mut self, id: &oxc_ast::ast::IdentifierReference<'a>) {
            if self.nested_function_depth == 0 && id.name.as_str() == "arguments" {
                self.found = true;
            }
        }
    }
    let mut finder = ArgsFinder::default();
    // Param defaults can reference `arguments` (sloppy mode); even
    // in strict mode, `function f(x = arguments) {}` is valid.
    for p in &params.items {
        if let Some(init) = p.initializer.as_deref() {
            finder.visit_expression(init);
        }
    }
    if let Some(rest) = &params.rest {
        finder.visit_binding_rest_element(&rest.rest);
    }
    if let Some(b) = body {
        for stmt in &b.statements {
            finder.visit_statement(stmt);
        }
    }
    finder.found
}

fn simple_formal_names(params: &oxc_ast::ast::FormalParameters<'_>) -> Vec<String> {
    params
        .items
        .iter()
        .filter_map(|param| match &param.pattern {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) if param.initializer.is_none() => {
                Some(id.name.to_string())
            }
            _ => None,
        })
        .collect()
}

fn mapped_formal_parameter_bindings(
    cx: &Compiler,
    params: &oxc_ast::ast::FormalParameters<'_>,
) -> Vec<MappedArgumentBinding> {
    let names = simple_formal_names(params);
    let mut seen = HashSet::new();
    let mut bindings = Vec::new();
    for (index, name) in names.iter().enumerate().rev() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(info) = cx.lookup_in_current_scope(name) else {
            continue;
        };
        bindings.push(MappedArgumentBinding {
            argument_index: index as u16,
            formal_name: name.clone(),
            storage: info.storage.to_argument_storage(),
        });
    }
    bindings.reverse();
    bindings
}

fn module_body_uses_top_level_await(stmts: &[Statement<'_>]) -> bool {
    use oxc_ast_visit::Visit;
    #[derive(Default)]
    struct AwaitFinder {
        depth: u32,
        found: bool,
    }
    impl<'a> Visit<'a> for AwaitFinder {
        fn visit_function(
            &mut self,
            it: &oxc_ast::ast::Function<'a>,
            flags: oxc_syntax::scope::ScopeFlags,
        ) {
            self.depth += 1;
            oxc_ast_visit::walk::walk_function(self, it, flags);
            self.depth -= 1;
        }
        fn visit_arrow_function_expression(
            &mut self,
            it: &oxc_ast::ast::ArrowFunctionExpression<'a>,
        ) {
            self.depth += 1;
            oxc_ast_visit::walk::walk_arrow_function_expression(self, it);
            self.depth -= 1;
        }
        fn visit_class(&mut self, it: &oxc_ast::ast::Class<'a>) {
            self.depth += 1;
            oxc_ast_visit::walk::walk_class(self, it);
            self.depth -= 1;
        }
        fn visit_await_expression(&mut self, it: &oxc_ast::ast::AwaitExpression<'a>) {
            if self.depth == 0 {
                self.found = true;
            }
            oxc_ast_visit::walk::walk_await_expression(self, it);
        }
    }
    let mut finder = AwaitFinder::default();
    for stmt in stmts {
        finder.visit_statement(stmt);
    }
    finder.found
}

/// Walk `stmts` collecting every `var`-declared name reachable
/// without crossing a function or class boundary. Per ECMA-262
/// §8.1.6 VarScopedDeclarations, these names belong to the
/// enclosing function (or script / module) variable environment.
///
/// Walks through:
/// - Block statements (`{ var x; }`).
/// - `if / else`, `while`, `do-while`, `for(;;)`, `for-in`, `for-of`,
///   `switch` cases, `try / catch / finally`, labelled statements.
/// - The init clause of `for(var ... ; ; )` and the head of
///   `for(var x in/of ...)`.
///
/// Stops at function / class declarations: their bodies own their
/// own variable scope.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-static-semantics-varscopeddeclarations>
fn hoist_var_names<'a>(stmts: &[Statement<'a>], out: &mut Vec<String>) {
    for stmt in stmts {
        hoist_var_names_in_stmt(stmt, out);
    }
}

fn hoist_var_names_in_stmt<'a>(stmt: &Statement<'a>, out: &mut Vec<String>) {
    match stmt {
        Statement::VariableDeclaration(d)
            if matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Var) =>
        {
            for declarator in d.declarations.iter() {
                collect_pattern_var_names(&declarator.id, out);
            }
        }
        // §16.2.3.7 ExportEntry — `export var x` shares the
        // module's `var`-hoisted scope: the name must be
        // pre-declared at the module-init top, exactly as for a
        // bare `var x`. The export side-effect (mirroring into
        // `module_env`) runs at the source position via the
        // export arm; here we only need to surface the name.
        Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
            if let Some(oxc_ast::ast::Declaration::VariableDeclaration(v)) = &decl.declaration
                && matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                for declarator in v.declarations.iter() {
                    collect_pattern_var_names(&declarator.id, out);
                }
            }
        }
        Statement::BlockStatement(b) => hoist_var_names(&b.body, out),
        Statement::IfStatement(s) => {
            hoist_var_names_in_stmt(&s.consequent, out);
            if let Some(alt) = &s.alternate {
                hoist_var_names_in_stmt(alt, out);
            }
        }
        Statement::WhileStatement(s) => hoist_var_names_in_stmt(&s.body, out),
        Statement::DoWhileStatement(s) => hoist_var_names_in_stmt(&s.body, out),
        Statement::ForStatement(s) => {
            if let Some(oxc_ast::ast::ForStatementInit::VariableDeclaration(d)) = &s.init
                && matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                for declarator in d.declarations.iter() {
                    collect_pattern_var_names(&declarator.id, out);
                }
            }
            hoist_var_names_in_stmt(&s.body, out);
        }
        Statement::ForInStatement(s) => {
            if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(d) = &s.left
                && matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                for declarator in d.declarations.iter() {
                    collect_pattern_var_names(&declarator.id, out);
                }
            }
            hoist_var_names_in_stmt(&s.body, out);
        }
        Statement::ForOfStatement(s) => {
            if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(d) = &s.left
                && matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            {
                for declarator in d.declarations.iter() {
                    collect_pattern_var_names(&declarator.id, out);
                }
            }
            hoist_var_names_in_stmt(&s.body, out);
        }
        Statement::SwitchStatement(s) => {
            for case in s.cases.iter() {
                hoist_var_names(&case.consequent, out);
            }
        }
        Statement::TryStatement(s) => {
            hoist_var_names(&s.block.body, out);
            if let Some(handler) = &s.handler {
                hoist_var_names(&handler.body.body, out);
            }
            if let Some(finalizer) = &s.finalizer {
                hoist_var_names(&finalizer.body, out);
            }
        }
        Statement::LabeledStatement(s) => hoist_var_names_in_stmt(&s.body, out),
        // `function`, `class`, plain expressions, etc. — none
        // contribute var-declared names to this scope.
        _ => {}
    }
}

/// Collect every binding identifier reachable from `pattern` —
/// supports plain identifiers and the destructuring patterns the
/// foundation accepts.
fn collect_pattern_var_names(pattern: &oxc_ast::ast::BindingPattern<'_>, out: &mut Vec<String>) {
    use oxc_ast::ast::BindingPattern;
    match pattern {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.as_str().to_string()),
        BindingPattern::ArrayPattern(p) => {
            for elem in p.elements.iter().flatten() {
                collect_pattern_var_names(elem, out);
            }
            if let Some(rest) = &p.rest {
                collect_pattern_var_names(&rest.argument, out);
            }
        }
        BindingPattern::ObjectPattern(p) => {
            for prop in p.properties.iter() {
                collect_pattern_var_names(&prop.value, out);
            }
            if let Some(rest) = &p.rest {
                collect_pattern_var_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(p) => collect_pattern_var_names(&p.left, out),
    }
}

/// Pre-declare each hoisted `var` name on the current scope per
/// §10.2.11 FunctionDeclarationInstantiation step 28: bind to
/// `undefined` with `[[Mutable]]`, no TDZ. Names that already live
/// in the current scope (formal parameters, `let`/`const` shadowing,
/// the function's self-name) are skipped — this matches §10.2.11
/// step 27 ("If the same name is bound by both a parameter and a
/// VarDeclaration, the parameter binding wins").
fn pre_declare_var_bindings(
    cx: &mut Compiler,
    names: &[String],
    span: (u32, u32),
) -> Result<(), CompileError> {
    let mut seen: HashSet<String> = HashSet::new();
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        if cx.lookup_binding(name).is_some() {
            // Already bound by a parameter or the function self-name —
            // §10.2.11 step 27.b leaves the existing binding intact.
            continue;
        }
        let storage = cx.declare_binding(name, false, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
        cx.emit_store_storage(dst, storage, span);
        cx.mark_initialized(name);
    }
    Ok(())
}

/// Walk `stmts` collecting every top-level lexical declaration name
/// — `let x`, `const x`, `class C` — that lives at the current
/// function / script / module scope. Top-level only: nested blocks,
/// loop bodies, etc., own their own block scope and aren't touched.
///
/// Names are returned with their declaration kind so the pre-pass
/// can call [`Compiler::declare_binding`] with the right `is_const`
/// flag.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-static-semantics-lexicallydeclarednames>
fn hoist_lexical_names(stmts: &[Statement<'_>], out: &mut Vec<(String, bool)>) {
    for stmt in stmts {
        match stmt {
            Statement::VariableDeclaration(d)
                if matches!(
                    d.kind,
                    oxc_ast::ast::VariableDeclarationKind::Let
                        | oxc_ast::ast::VariableDeclarationKind::Const
                ) =>
            {
                collect_lexical_var_names(d, out);
            }
            Statement::ClassDeclaration(c) => {
                if let Some(id) = &c.id {
                    out.push((id.name.as_str().to_string(), false));
                }
            }
            // §16.2.3.7 — `export let x` / `export const x` /
            // `export class C` introduce a fresh module-scope
            // lexical binding that must be pre-declared in TDZ
            // before module-init runs, just like any other
            // top-level lexical name. `export function` is
            // handled by [`hoist_function_declarations`].
            //
            // `export var x` is a `var`-scoped binding picked up
            // by [`hoist_var_names_in_stmt`]; we explicitly skip
            // it here so the name doesn't end up double-bound.
            Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
                match &decl.declaration {
                    Some(oxc_ast::ast::Declaration::VariableDeclaration(v))
                        if matches!(
                            v.kind,
                            oxc_ast::ast::VariableDeclarationKind::Let
                                | oxc_ast::ast::VariableDeclarationKind::Const
                        ) =>
                    {
                        collect_lexical_var_names(v, out);
                    }
                    Some(oxc_ast::ast::Declaration::ClassDeclaration(c)) => {
                        if let Some(id) = &c.id {
                            out.push((id.name.as_str().to_string(), false));
                        }
                    }
                    _ => {}
                }
            }
            // `export default class C {}` and `export default
            // expression` do not contribute a top-level lexical
            // name in the foundation slice — the value lives on
            // `module_env.default` and the source-position
            // [`Statement::ExportDefaultDeclaration`] arm wires
            // that store. Per §15.2.3.5 a *named* default class
            // creates a module-scope binding `C`; that binding is
            // a separate spec slice and is filed as a follow-up.
            // `export default function f(){}` is a hoistable
            // declaration; its name lands at the top of
            // [`hoist_function_declarations`].
            // Don't recurse into blocks / control-flow constructs:
            // those declarations belong to the inner block scope,
            // not the enclosing function / module body.
            _ => {}
        }
    }
}

/// Push every plain-identifier name declared by a `let`/`const`
/// declaration into `out` with its `is_const` flag. Shared
/// between the source-statement arm and the
/// `ExportNamedDeclaration(VariableDeclaration)` arm so both pre-
/// hoist passes apply identical rules to the inner declaration.
fn collect_lexical_var_names(
    d: &oxc_ast::ast::VariableDeclaration<'_>,
    out: &mut Vec<(String, bool)>,
) {
    let is_const = matches!(d.kind, oxc_ast::ast::VariableDeclarationKind::Const);
    for declarator in d.declarations.iter() {
        // Only pre-hoist plain identifier bindings; destructuring
        // patterns declare each leaf at their source position via
        // `destructure_into`. A hoisted nested function that
        // captures a destructured leaf name is filed as a
        // follow-up.
        if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
            out.push((id.name.as_str().to_string(), is_const));
        }
    }
}

/// Pre-declare each top-level lexical name from
/// [`hoist_lexical_names`] so inner function declarations (which
/// hoist *above* the lexical declarations) can resolve their
/// captures. Bindings start in TDZ — the source-level `let` /
/// `const` / `class` arm flips them to initialized once the
/// initialiser stores its value.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-functiondeclarationinstantiation>
///   step 33 (CreateMutableBinding for `let`, CreateImmutableBinding
///   for `const`).
fn pre_declare_lexical_bindings(
    cx: &mut Compiler,
    names: &[(String, bool)],
    span: (u32, u32),
) -> Result<(), CompileError> {
    for (name, is_const) in names {
        if cx.lookup_binding(name).is_some() {
            // Already pre-declared (parameter, var-hoist clash, …).
            // §10.2.11 forbids let / const / class from shadowing a
            // var-hoisted name at the same scope; surface a clear
            // diagnostic at declaration time rather than here.
            continue;
        }
        cx.declare_binding(name, *is_const, span)?;
    }
    Ok(())
}

/// Hoist top-level `function f() {…}` declarations from `stmts` to
/// the start of the current function / script / module scope, per
/// ECMA-262 §10.2.11 FunctionDeclarationInstantiation step 30.
///
/// # Algorithm
/// 1. Find every `FunctionDeclaration` in the *direct* statement
///    list. Per §10.2.11 step 14 the LAST declaration with a given
///    name wins — earlier siblings are pre-empted at the binding
///    site (their bytecode still emits because OXC parses each
///    independently, but only the last store survives).
/// 2. For each surviving declaration: pre-declare its name, compile
///    the function body, materialise the closure value, store it
///    into the binding, and mark the binding initialised. Record the
///    name in `cx.hoisted_function_names` so the source-position
///    arm in `compile_statement` becomes a no-op.
///
/// Block-nested `FunctionDeclaration`s (`if (true) { function f(){} }`)
/// are *not* hoisted by this pass — the foundation models them as
/// block-scoped per the ES strict-mode rule. Annex B sloppy-mode
/// extensions are out of scope.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-functiondeclarationinstantiation>
/// - <https://tc39.es/ecma262/#sec-globaldeclarationinstantiation>
fn hoist_function_declarations(
    cx: &mut Compiler,
    stmts: &[Statement<'_>],
) -> Result<(), CompileError> {
    use std::collections::HashMap;
    // Resolve each statement to its hoistable `FunctionDeclaration`
    // payload, including the export-wrapped forms `export function`
    // and `export default function`. Returns `None` for other
    // statements and for `function f.declare`-only TS shapes.
    fn hoistable_function<'b, 'a: 'b>(
        stmt: &'b Statement<'a>,
    ) -> Option<&'b oxc_ast::ast::Function<'a>> {
        match stmt {
            Statement::FunctionDeclaration(f) if !f.declare => Some(f),
            Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
                if let Some(oxc_ast::ast::Declaration::FunctionDeclaration(f)) = &decl.declaration
                    && !f.declare
                {
                    Some(f)
                } else {
                    None
                }
            }
            // §15.2 ExportDefaultDeclaration with a
            // `FunctionDeclaration` payload hoists when the
            // function carries a binding identifier (per HoistableDeclaration).
            // Anonymous default functions are evaluated at the
            // export's source position by the export arm; they
            // don't introduce a module-scope name.
            Statement::ExportDefaultDeclaration(decl) => {
                if let oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) =
                    &decl.declaration
                    && !f.declare
                    && f.id.is_some()
                {
                    Some(f)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // Pass 1 — last-occurrence-wins: identify the surviving
    // declaration index per name.
    let mut last_idx: HashMap<String, usize> = HashMap::new();
    for (idx, stmt) in stmts.iter().enumerate() {
        if let Some(f) = hoistable_function(stmt)
            && let Some(id) = &f.id
        {
            last_idx.insert(id.name.as_str().to_string(), idx);
        }
    }
    // Pass 2 — pre-declare each surviving name in the current scope
    // so mutual references between hoisted siblings (and forward
    // references from inner functions) all resolve before any body
    // is compiled. The binding is initialised to undefined; the
    // closure value lands in pass 3.
    for (idx, stmt) in stmts.iter().enumerate() {
        let Some(f) = hoistable_function(stmt) else {
            continue;
        };
        let Some(id) = &f.id else {
            continue;
        };
        let name = id.name.as_str().to_string();
        if last_idx.get(&name) != Some(&idx) {
            continue;
        }
        let span = (f.span.start, f.span.end);
        if cx.lookup_binding(&name).is_none() {
            let storage = cx.declare_binding(&name, false, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
            cx.emit_store_storage(dst, storage, span);
            cx.mark_initialized(&name);
        }
        cx.top_mut().hoisted_function_names.insert(name);
    }
    // Pass 3 — compile each surviving function body and store the
    // resulting closure into the pre-bound slot. With every name
    // already declared, mutually-recursive declarations bind
    // correctly regardless of source order.
    for (idx, stmt) in stmts.iter().enumerate() {
        let Some(f) = hoistable_function(stmt) else {
            continue;
        };
        let Some(id) = &f.id else {
            continue;
        };
        let name = id.name.as_str().to_string();
        if last_idx.get(&name) != Some(&idx) {
            continue;
        }
        let span = (f.span.start, f.span.end);
        let (function_id, captures) = compile_function_full(
            cx,
            &name,
            &f.params,
            &f.body,
            span,
            f.r#async,
            f.generator,
            false,
        )?;
        let const_idx = cx.intern_function_id(function_id);
        let tmp = cx.alloc_scratch();
        emit_make_callable(cx, tmp, const_idx, &captures, false, span);
        let storage = cx
            .lookup_binding(&name)
            .expect("pass 2 pre-declared the binding")
            .storage;
        cx.emit_store_storage(tmp, storage, span);
        // Mirror through to `module_env` for `export function f`
        // (and `export default function f` — its export entry
        // landed under `default` from the pre-pass; the named
        // mirror is harmless for non-exported names because
        // `emit_module_export_mirror` filters on
        // `module_state.exported_names`).
        cx.emit_module_export_mirror(&name, tmp, span);
        // §15.2 — `export default function f(){}` also requires
        // `module_env.default = f`. Detect by walking the source
        // statement: when the surviving declaration came from an
        // export-default the `default` mirror must fire too.
        if matches!(stmts.get(idx), Some(Statement::ExportDefaultDeclaration(_))) {
            cx.emit_module_export_default_mirror(tmp, span);
        }
    }
    Ok(())
}

fn compile_function_full(
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
        vec![Operand::Register(tmp), Operand::ConstIndex(const_idx)],
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
        parent.emit(Op::CollectArguments, vec![Operand::Register(tmp)], span);
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

/// Lower an [`AssignmentExpression`]. Plain `=` and the compound
/// arithmetic / bitwise / `**=` shapes share one path; logical
/// assignments (`||=`, `&&=`, `??=`) are deferred (they need
/// short-circuit lowering) and short-circuit out with a clear
/// `Unsupported` diagnostic.
fn compile_assignment(
    cx: &mut Compiler,
    a: &oxc_ast::ast::AssignmentExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (a.span.start, a.span.end);
    let compound_op = compound_assign_op(a.operator);
    if a.operator.is_logical() {
        // §13.15.4 LogicalAssignment — `x &&= y`, `x ||= y`, `x ??= y`.
        // Lowered to the desugared form:
        //   `x &&= y` → if (cur) x = y;
        //   `x ||= y` → if (!cur) x = y;
        //   `x ??= y` → if (cur is null/undefined) x = y;
        // Foundation handles the identifier-target fast path here;
        // member / computed-member targets fall through to the
        // existing assignment branches via a synthesised plain-`=`.
        return compile_logical_assignment(cx, a, span);
    }
    if let AssignmentTarget::StaticMemberExpression(member) = &a.left {
        let obj_reg = compile_expr(cx, &member.object, span)?;
        let name_idx = cx.intern_string_constant(member.property.name.as_str());
        let new_value = match compound_op {
            None => compile_expr(cx, &a.right, span)?,
            Some(op) => {
                let current = cx.alloc_scratch();
                cx.emit(
                    Op::LoadProperty,
                    vec![
                        Operand::Register(current),
                        Operand::Register(obj_reg),
                        Operand::ConstIndex(name_idx),
                    ],
                    span,
                );
                let rhs = compile_expr(cx, &a.right, span)?;
                let dst = cx.alloc_scratch();
                cx.emit(
                    op,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(current),
                        Operand::Register(rhs),
                    ],
                    span,
                );
                dst
            }
        };
        let store_scratch = cx.alloc_scratch();
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_idx),
                Operand::Register(new_value),
                Operand::Register(store_scratch),
            ],
            span,
        );
        return Ok(new_value);
    }
    if let AssignmentTarget::PrivateFieldExpression(member) = &a.left {
        let mangled =
            cx.mangle_private(member.field.name.as_str())
                .ok_or(CompileError::Unsupported {
                    node: "PrivateFieldExpression assignment outside any class body".to_string(),
                    span,
                })?;
        let obj_reg = compile_expr(cx, &member.object, span)?;
        let name_idx = cx.intern_string_constant(&mangled);
        let new_value = match compound_op {
            None => compile_expr(cx, &a.right, span)?,
            Some(op) => {
                let current = cx.alloc_scratch();
                cx.emit(
                    Op::LoadProperty,
                    vec![
                        Operand::Register(current),
                        Operand::Register(obj_reg),
                        Operand::ConstIndex(name_idx),
                    ],
                    span,
                );
                let rhs = compile_expr(cx, &a.right, span)?;
                let dst = cx.alloc_scratch();
                cx.emit(
                    op,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(current),
                        Operand::Register(rhs),
                    ],
                    span,
                );
                dst
            }
        };
        let store_scratch = cx.alloc_scratch();
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_idx),
                Operand::Register(new_value),
                Operand::Register(store_scratch),
            ],
            span,
        );
        return Ok(new_value);
    }
    if let AssignmentTarget::ComputedMemberExpression(member) = &a.left {
        let arr_reg = compile_expr(cx, &member.object, span)?;
        let idx_reg = compile_expr(cx, &member.expression, span)?;
        let new_value = match compound_op {
            None => compile_expr(cx, &a.right, span)?,
            Some(op) => {
                let current = cx.alloc_scratch();
                cx.emit(
                    Op::LoadElement,
                    vec![
                        Operand::Register(current),
                        Operand::Register(arr_reg),
                        Operand::Register(idx_reg),
                    ],
                    span,
                );
                let rhs = compile_expr(cx, &a.right, span)?;
                let dst = cx.alloc_scratch();
                cx.emit(
                    op,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(current),
                        Operand::Register(rhs),
                    ],
                    span,
                );
                dst
            }
        };
        cx.emit_store_element(arr_reg, idx_reg, new_value, span);
        return Ok(new_value);
    }
    // §13.15.5 DestructuringAssignmentEvaluation — array / object
    // destructuring assignment targets recurse through the helper.
    if let AssignmentTarget::ArrayAssignmentTarget(arr) = &a.left {
        let value_reg = compile_expr(cx, &a.right, span)?;
        assign_array_pattern(cx, arr, value_reg, span)?;
        return Ok(value_reg);
    }
    if let AssignmentTarget::ObjectAssignmentTarget(o) = &a.left {
        let value_reg = compile_expr(cx, &a.right, span)?;
        assign_object_pattern(cx, o, value_reg, span)?;
        return Ok(value_reg);
    }
    // `name = value` — local or captured-upvalue store.
    let name = match &a.left {
        AssignmentTarget::AssignmentTargetIdentifier(id) => id.name.as_str().to_string(),
        _ => {
            return Err(CompileError::Unsupported {
                node: "AssignmentTarget (non-identifier)".to_string(),
                span,
            });
        }
    };
    let storage = match cx.lookup_binding(&name) {
        Some(info) if info.is_const => {
            return Err(CompileError::Unsupported {
                node: format!("assignment to const `{name}`"),
                span,
            });
        }
        Some(info) => Some(info.storage),
        // §10.2.4.1 PutValue fallback — assignment to an undeclared
        // identifier in sloppy mode creates a property on the
        // global object. Foundation lowers this as a `StoreProperty`
        // against `globalThis` so harness-style code that pre-
        // populates globals (e.g. `assert.sameValue = function …`
        // before the first reference) keeps working.
        // <https://tc39.es/ecma262/#sec-putvalue>
        None => cx
            .resolve_capture(&name)
            .map(|idx| BindingStorage::Upvalue { idx }),
    };
    let value = match compound_op {
        None => compile_expr(cx, &a.right, span)?,
        Some(op) => {
            let current = cx.alloc_scratch();
            match storage {
                Some(s) => cx.emit_load_storage(current, s, span),
                None => {
                    let global = cx.alloc_scratch();
                    cx.emit(Op::LoadGlobalThis, vec![Operand::Register(global)], span);
                    cx.emit_load_property(current, global, &name, span);
                }
            }
            let rhs = compile_expr(cx, &a.right, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                op,
                vec![
                    Operand::Register(dst),
                    Operand::Register(current),
                    Operand::Register(rhs),
                ],
                span,
            );
            dst
        }
    };
    match storage {
        Some(s) => {
            cx.emit_store_storage(value, s, span);
            cx.mark_initialized(&name);
            cx.emit_module_export_mirror(&name, value, span);
        }
        None => {
            // Store to the globalThis property table.
            let global = cx.alloc_scratch();
            cx.emit(Op::LoadGlobalThis, vec![Operand::Register(global)], span);
            let name_idx = cx.intern_string_constant(&name);
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(global),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(value),
                    Operand::Register(scratch),
                ],
                span,
            );
        }
    }
    Ok(value)
}

/// §13.15.4 LogicalAssignment — `x &&= y`, `x ||= y`, `x ??= y`.
fn compile_logical_assignment(
    cx: &mut Compiler,
    a: &oxc_ast::ast::AssignmentExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    use oxc_ast::ast::AssignmentOperator;
    // Read current value of the target (load only; no store yet).
    let cur = match &a.left {
        AssignmentTarget::AssignmentTargetIdentifier(id) => {
            let name = id.name.as_str().to_string();
            let load = cx.alloc_scratch();
            if let Some(info) = cx.lookup_binding(&name) {
                cx.emit_load_storage(load, info.storage, span);
            } else if let Some(idx) = cx.resolve_capture(&name) {
                cx.emit_load_storage(load, BindingStorage::Upvalue { idx }, span);
            } else {
                let global = cx.alloc_scratch();
                cx.emit(Op::LoadGlobalThis, vec![Operand::Register(global)], span);
                cx.emit_load_property(load, global, &name, span);
            }
            load
        }
        AssignmentTarget::StaticMemberExpression(m) => {
            let obj_reg = compile_expr(cx, &m.object, span)?;
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            let load = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(load),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            load
        }
        AssignmentTarget::ComputedMemberExpression(m) => {
            let obj_reg = compile_expr(cx, &m.object, span)?;
            let key_reg = compile_expr(cx, &m.expression, span)?;
            let load = cx.alloc_scratch();
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(load),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            load
        }
        other => {
            return Err(CompileError::Unsupported {
                node: format!("LogicalAssignment target ({other:?})"),
                span,
            });
        }
    };
    // Compute the test condition. For `&&=`, jump-if-false skips
    // the assignment. For `||=`, the `!` inverts so we use
    // jump-if-true. For `??=`, test "cur is null or undefined".
    let test_reg = match a.operator {
        AssignmentOperator::LogicalAnd => {
            // `&&=` — assign only when cur is truthy. Test is cur.
            let bool_r = cx.alloc_scratch();
            cx.emit(
                Op::ToBoolean,
                vec![Operand::Register(bool_r), Operand::Register(cur)],
                span,
            );
            bool_r
        }
        AssignmentOperator::LogicalOr => {
            // `||=` — assign only when cur is falsy. Test is !cur.
            let bool_r = cx.alloc_scratch();
            cx.emit(
                Op::ToBoolean,
                vec![Operand::Register(bool_r), Operand::Register(cur)],
                span,
            );
            let not_r = cx.alloc_scratch();
            cx.emit(
                Op::LogicalNot,
                vec![Operand::Register(not_r), Operand::Register(bool_r)],
                span,
            );
            not_r
        }
        AssignmentOperator::LogicalNullish => {
            // `??=` — assign only when cur is null/undefined.
            // Compare cur === null || cur === undefined.
            let undef_r = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, vec![Operand::Register(undef_r)], span);
            let null_r = cx.alloc_scratch();
            cx.emit(Op::LoadNull, vec![Operand::Register(null_r)], span);
            let eq_undef = cx.alloc_scratch();
            cx.emit(
                Op::Equal,
                vec![
                    Operand::Register(eq_undef),
                    Operand::Register(cur),
                    Operand::Register(undef_r),
                ],
                span,
            );
            let eq_null = cx.alloc_scratch();
            cx.emit(
                Op::Equal,
                vec![
                    Operand::Register(eq_null),
                    Operand::Register(cur),
                    Operand::Register(null_r),
                ],
                span,
            );
            // OR them via boolean-logic: if eq_undef → true; else
            // result = eq_null. We use a register-merge pattern.
            let merged = cx.alloc_scratch();
            cx.emit(
                Op::ToBoolean,
                vec![Operand::Register(merged), Operand::Register(eq_undef)],
                span,
            );
            // `merged = merged || eq_null`. The simplest is a
            // sequence: jump if merged true; else copy eq_null.
            let jump_if_true = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(merged), span);
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(eq_null), Operand::Imm32(merged as i32)],
                span,
            );
            cx.patch_branch_to_here(jump_if_true);
            merged
        }
        _ => unreachable!("non-logical operator in compile_logical_assignment"),
    };
    // result = cur initially. Skip the assignment when test is
    // false.
    let result = cx.alloc_scratch();
    cx.emit(
        Op::StoreLocal,
        vec![Operand::Register(cur), Operand::Imm32(result as i32)],
        span,
    );
    let skip = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(test_reg), span);
    // Assignment branch: synthesize a plain-`=` and re-enter
    // assign_to_target.
    let new_value = compile_expr(cx, &a.right, span)?;
    assign_to_target(cx, &a.left, new_value, span)?;
    cx.emit(
        Op::StoreLocal,
        vec![Operand::Register(new_value), Operand::Imm32(result as i32)],
        span,
    );
    cx.patch_branch_to_here(skip);
    Ok(result)
}

/// §13.15.5 DestructuringAssignmentEvaluation — recurse into a
/// destructuring assignment target and store the relevant slices
/// of `value_reg` into each leaf.
///
/// Foundation handles the common shapes used across the test262
/// corpus: simple identifier leaves, nested array / object
/// destructuring, defaults via `=`, and rest elements (collected
/// via `Op::CollectRest`). Computed property keys recurse through
/// the runtime.
fn assign_to_target(
    cx: &mut Compiler,
    target: &oxc_ast::ast::AssignmentTarget<'_>,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    use oxc_ast::ast::AssignmentTarget;
    match target {
        AssignmentTarget::ArrayAssignmentTarget(arr) => {
            assign_array_pattern(cx, arr, value_reg, span)
        }
        AssignmentTarget::ObjectAssignmentTarget(obj) => {
            assign_object_pattern(cx, obj, value_reg, span)
        }
        AssignmentTarget::AssignmentTargetIdentifier(id) => {
            let name = id.name.as_str().to_string();
            store_identifier(cx, &name, value_reg, span)
        }
        AssignmentTarget::StaticMemberExpression(member) => {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(value_reg),
                    Operand::Register(scratch),
                ],
                span,
            );
            Ok(())
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let key_reg = compile_expr(cx, &member.expression, span)?;
            cx.emit_store_element(obj_reg, key_reg, value_reg, span);
            Ok(())
        }
        other => Err(CompileError::Unsupported {
            node: format!("AssignmentTarget ({other:?})"),
            span,
        }),
    }
}

/// Apply `value_reg` to a `ArrayAssignmentTarget`. Walks each
/// element, reads `value[i]` via `Op::LoadElement`, and recurses
/// into the element's target. Defaults (`= expr`) substitute when
/// the element is `undefined`. Rest elements (`...rest`) collect
/// the trailing slice via `Op::CollectRest`.
fn assign_array_pattern(
    cx: &mut Compiler,
    arr: &oxc_ast::ast::ArrayAssignmentTarget<'_>,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    for (idx, element) in arr.elements.iter().enumerate() {
        let Some(element) = element else { continue };
        let elem_span = span;
        let idx_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadInt32,
            vec![Operand::Register(idx_reg), Operand::Imm32(idx as i32)],
            elem_span,
        );
        let val_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadElement,
            vec![
                Operand::Register(val_reg),
                Operand::Register(value_reg),
                Operand::Register(idx_reg),
            ],
            elem_span,
        );
        assign_maybe_default(cx, element, val_reg, elem_span)?;
    }
    if let Some(rest) = arr.rest.as_ref() {
        // Foundation: collect the trailing slice via CollectRest
        // (already used by parameter rest binding) into a fresh
        // array, then assign into the rest target.
        let start_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadInt32,
            vec![
                Operand::Register(start_reg),
                Operand::Imm32(arr.elements.len() as i32),
            ],
            span,
        );
        let collected = cx.alloc_scratch();
        cx.emit(
            Op::CollectRest,
            vec![
                Operand::Register(collected),
                Operand::Register(value_reg),
                Operand::Register(start_reg),
            ],
            span,
        );
        assign_to_target(cx, &rest.target, collected, span)?;
    }
    Ok(())
}

/// Apply `value_reg` to an `ObjectAssignmentTarget`.
fn assign_object_pattern(
    cx: &mut Compiler,
    obj: &oxc_ast::ast::ObjectAssignmentTarget<'_>,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    use oxc_ast::ast::{AssignmentTargetProperty, PropertyKey};
    enum ExtractedKey {
        Static(String),
        Runtime(u16),
    }
    let mut extracted_keys: Vec<ExtractedKey> = Vec::new();
    for prop in &obj.properties {
        match prop {
            AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                let pspan = span;
                let name = p.binding.name.as_str().to_string();
                let val = cx.alloc_scratch();
                cx.emit_load_property(val, value_reg, &name, pspan);
                let final_val = match p.init.as_ref() {
                    Some(default) => apply_default(cx, val, default, pspan)?,
                    None => val,
                };
                store_identifier(cx, &name, final_val, pspan)?;
                extracted_keys.push(ExtractedKey::Static(name));
            }
            AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                let pspan = span;
                let val = cx.alloc_scratch();
                if p.computed {
                    let key_reg = match &p.name {
                        PropertyKey::StaticIdentifier(id) => {
                            let r = cx.alloc_scratch();
                            let s = cx.intern_string_constant(id.name.as_str());
                            cx.emit(
                                Op::LoadString,
                                vec![Operand::Register(r), Operand::ConstIndex(s)],
                                pspan,
                            );
                            r
                        }
                        _ => compile_expr_as_property_key(cx, &p.name, pspan)?,
                    };
                    cx.emit(
                        Op::LoadElement,
                        vec![
                            Operand::Register(val),
                            Operand::Register(value_reg),
                            Operand::Register(key_reg),
                        ],
                        pspan,
                    );
                    extracted_keys.push(ExtractedKey::Runtime(key_reg));
                } else {
                    let key_str = match &p.name {
                        PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                        PropertyKey::StringLiteral(lit) => lit.value.to_string(),
                        PropertyKey::NumericLiteral(lit) => {
                            numeric_literal_to_property_key(lit.value)
                        }
                        PropertyKey::BigIntLiteral(lit) => lit
                            .raw
                            .as_ref()
                            .map(|s| s.as_str())
                            .unwrap_or("")
                            .trim_end_matches('n')
                            .to_string(),
                        other => {
                            return Err(CompileError::Unsupported {
                                node: format!("ObjectAssignmentTarget: non-string key ({other:?})"),
                                span: pspan,
                            });
                        }
                    };
                    cx.emit_load_property(val, value_reg, &key_str, pspan);
                    extracted_keys.push(ExtractedKey::Static(key_str));
                }
                assign_maybe_default(cx, &p.binding, val, pspan)?;
            }
        }
    }
    if let Some(rest) = obj.rest.as_ref() {
        // §13.15.5 RestObjectAssignment — same shape as the
        // BindingPattern variant.
        let rest_obj = cx.alloc_scratch();
        cx.emit(Op::NewObject, vec![Operand::Register(rest_obj)], span);
        let scratch = cx.alloc_scratch();
        cx.emit(
            Op::ObjectCall,
            vec![
                Operand::Register(scratch),
                Operand::ConstIndex(otter_bytecode::method_id::ObjectMethod::Assign.as_u32()),
                Operand::ConstIndex(2),
                Operand::Register(rest_obj),
                Operand::Register(value_reg),
            ],
            span,
        );
        for key in &extracted_keys {
            match key {
                ExtractedKey::Static(s) => {
                    let key_const = cx.intern_string_constant(s);
                    let del_dst = cx.alloc_scratch();
                    cx.emit(
                        Op::DeleteProperty,
                        vec![
                            Operand::Register(del_dst),
                            Operand::Register(rest_obj),
                            Operand::ConstIndex(key_const),
                        ],
                        span,
                    );
                }
                ExtractedKey::Runtime(key_reg) => {
                    let del_dst = cx.alloc_scratch();
                    cx.emit(
                        Op::DeleteElement,
                        vec![
                            Operand::Register(del_dst),
                            Operand::Register(rest_obj),
                            Operand::Register(*key_reg),
                        ],
                        span,
                    );
                }
            }
        }
        assign_to_target(cx, &rest.target, rest_obj, span)?;
    }
    Ok(())
}

fn assign_maybe_default(
    cx: &mut Compiler,
    target: &oxc_ast::ast::AssignmentTargetMaybeDefault<'_>,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    use oxc_ast::ast::AssignmentTargetMaybeDefault;
    match target {
        AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
            let resolved = apply_default(cx, value_reg, &d.init, span)?;
            assign_to_target(cx, &d.binding, resolved, span)
        }
        other => {
            let inner = other
                .as_assignment_target()
                .ok_or_else(|| CompileError::Unsupported {
                    node: format!("AssignmentTargetMaybeDefault ({other:?})"),
                    span,
                })?;
            assign_to_target(cx, inner, value_reg, span)
        }
    }
}

/// `value_reg === undefined ? init : value_reg`. Foundation
/// emits the conditional load via JumpIfFalse on
/// `typeof value_reg === "undefined"`.
fn apply_default(
    cx: &mut Compiler,
    value_reg: u16,
    init: &oxc_ast::ast::Expression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    // Test `value_reg !== undefined` and pick.
    let tag_reg = cx.alloc_scratch();
    cx.emit(
        Op::TypeOf,
        vec![Operand::Register(tag_reg), Operand::Register(value_reg)],
        span,
    );
    let undef_str_reg = cx.alloc_scratch();
    let undef_const = cx.intern_string_constant("undefined");
    cx.emit(
        Op::LoadString,
        vec![
            Operand::Register(undef_str_reg),
            Operand::ConstIndex(undef_const),
        ],
        span,
    );
    let is_undef = cx.alloc_scratch();
    cx.emit(
        Op::Equal,
        vec![
            Operand::Register(is_undef),
            Operand::Register(tag_reg),
            Operand::Register(undef_str_reg),
        ],
        span,
    );
    let result = cx.alloc_scratch();
    // Default branch: if !is_undef (i.e. value defined) jump to
    // the "use value" arm; fall through into "use init".
    let jump_to_use_value = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(is_undef), span);
    let init_val = compile_expr(cx, init, span)?;
    cx.emit(
        Op::StoreLocal,
        vec![Operand::Register(init_val), Operand::Imm32(result as i32)],
        span,
    );
    let jump_to_end = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch_to_here(jump_to_use_value);
    cx.emit(
        Op::StoreLocal,
        vec![Operand::Register(value_reg), Operand::Imm32(result as i32)],
        span,
    );
    cx.patch_branch_to_here(jump_to_end);
    Ok(result)
}

/// Store `value_reg` into the binding (or globalThis) for `name`.
/// Mirrors the identifier-store branch of `compile_assignment` but
/// without the compound-op handling.
fn store_identifier(
    cx: &mut Compiler,
    name: &str,
    value_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let storage = match cx.lookup_binding(name) {
        Some(info) if info.is_const => {
            return Err(CompileError::Unsupported {
                node: format!("destructuring assignment to const `{name}`"),
                span,
            });
        }
        Some(info) => Some(info.storage),
        None => cx
            .resolve_capture(name)
            .map(|idx| BindingStorage::Upvalue { idx }),
    };
    match storage {
        Some(s) => {
            cx.emit_store_storage(value_reg, s, span);
            cx.mark_initialized(name);
            cx.emit_module_export_mirror(name, value_reg, span);
        }
        None => {
            // §10.2.4.1 PutValue fallback to globalThis.
            let global = cx.alloc_scratch();
            cx.emit(Op::LoadGlobalThis, vec![Operand::Register(global)], span);
            let name_idx = cx.intern_string_constant(name);
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(global),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(value_reg),
                    Operand::Register(scratch),
                ],
                span,
            );
        }
    }
    Ok(())
}

/// Map a compound `AssignmentOperator` to the bytecode binop used
/// by `compile_assignment`. Returns `None` for plain `=` (which
/// skips the load-modify-store sequence) and for logical assigns
/// which the foundation slice doesn't lower yet.
fn compound_assign_op(op: AssignmentOperator) -> Option<Op> {
    Some(match op {
        AssignmentOperator::Assign => return None,
        AssignmentOperator::Addition => Op::Add,
        AssignmentOperator::Subtraction => Op::Sub,
        AssignmentOperator::Multiplication => Op::Mul,
        AssignmentOperator::Division => Op::Div,
        AssignmentOperator::Remainder => Op::Rem,
        AssignmentOperator::Exponential => Op::Pow,
        AssignmentOperator::ShiftLeft => Op::Shl,
        AssignmentOperator::ShiftRight => Op::Shr,
        AssignmentOperator::ShiftRightZeroFill => Op::Ushr,
        AssignmentOperator::BitwiseOR => Op::BitwiseOr,
        AssignmentOperator::BitwiseXOR => Op::BitwiseXor,
        AssignmentOperator::BitwiseAnd => Op::BitwiseAnd,
        AssignmentOperator::LogicalOr
        | AssignmentOperator::LogicalAnd
        | AssignmentOperator::LogicalNullish => return None,
    })
}

/// Lower one positional formal parameter at ordinal `ordinal` (the
/// raw argv slot the call dispatcher writes into).
///
/// OXC keeps the default expression on `FormalParameter::initializer`
/// rather than wrapping the pattern in an `AssignmentPattern`
/// (which is reserved for *inner* defaults like
/// `function f({x = 1}) {}`). We honour both spellings here so
/// callers don't have to peek into the OXC structure.
fn compile_formal_parameter(
    parent: &mut Compiler,
    ordinal: u16,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    initializer: Option<&Expression<'_>>,
    span: (u32, u32),
    allow_duplicate_formals: bool,
) -> Result<(), CompileError> {
    if initializer.is_none()
        && let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = pattern
    {
        return bind_simple_formal_parameter(
            parent,
            ordinal,
            id.name.as_str(),
            span,
            allow_duplicate_formals,
        );
    }
    if let Some(default_expr) = initializer {
        apply_default_into(parent, ordinal, default_expr, span)?;
    }
    if let oxc_ast::ast::BindingPattern::AssignmentPattern(asgn) = pattern {
        apply_default_into(
            parent,
            ordinal,
            &asgn.right,
            (asgn.span.start, asgn.span.end),
        )?;
        return destructure_into(parent, ordinal, &asgn.left, span);
    }
    destructure_into(parent, ordinal, pattern, span)
}

fn bind_simple_formal_parameter(
    parent: &mut Compiler,
    ordinal: u16,
    name: &str,
    span: (u32, u32),
    allow_duplicate_formals: bool,
) -> Result<(), CompileError> {
    let storage = if allow_duplicate_formals {
        match parent.lookup_in_current_scope(name) {
            Some(info) => info.storage,
            None => parent.declare_binding(name, false, span)?,
        }
    } else {
        parent.declare_binding(name, false, span)?
    };
    parent.emit_store_storage(ordinal, storage, span);
    parent.mark_initialized(name);
    Ok(())
}

fn formal_parameters_are_simple(params: &oxc_ast::ast::FormalParameters<'_>) -> bool {
    params.rest.is_none()
        && params.items.iter().all(|param| {
            param.initializer.is_none()
                && matches!(
                    param.pattern,
                    oxc_ast::ast::BindingPattern::BindingIdentifier(_)
                )
        })
}

fn validate_formal_parameter_names(
    params: &oxc_ast::ast::FormalParameters<'_>,
    is_strict: bool,
    allow_duplicates: bool,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let mut names = Vec::new();
    for param in &params.items {
        collect_pattern_var_names(&param.pattern, &mut names);
    }
    if let Some(rest) = &params.rest {
        collect_pattern_var_names(&rest.rest.argument, &mut names);
    }

    let mut seen = HashSet::new();
    for name in names {
        if is_strict && (name == "eval" || name == "arguments") {
            return Err(CompileError::Unsupported {
                node: format!("restricted formal parameter name `{name}` in strict function"),
                span,
            });
        }
        if !allow_duplicates && !seen.insert(name.clone()) {
            return Err(CompileError::Unsupported {
                node: format!("redeclaration of `{name}` in same scope"),
                span,
            });
        }
    }
    Ok(())
}

/// Lower the rest parameter (`function f(..., ...rest) { … }`).
/// Reads the trailing args off the frame via `Op::CollectRest`,
/// then routes the resulting array through the same
/// destructuring path so `function f(...[a, b])` falls out for
/// free.
fn compile_rest_parameter(
    parent: &mut Compiler,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let rest_reg = parent.alloc_scratch();
    parent.emit(Op::CollectRest, vec![Operand::Register(rest_reg)], span);
    destructure_into(parent, rest_reg, pattern, span)
}

/// Overwrite `value_reg` with the lazy default value when its
/// current contents are `undefined`. Compiles to:
///
/// ```text
///   ToBoolean tmp <- undefined?  ; using JumpIfNotUndefined-style
///   actually: equality compare with undefined + branch
/// ```
///
/// Foundation lowering uses two existing opcodes — `LoadUndefined`
/// and `Equal` followed by `JumpIfFalse` — to avoid introducing a
/// dedicated "is-undefined" branch.
fn apply_default_into(
    parent: &mut Compiler,
    value_reg: u16,
    default_expr: &Expression<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let undef_reg = parent.alloc_scratch();
    parent.emit(Op::LoadUndefined, vec![Operand::Register(undef_reg)], span);
    let cond_reg = parent.alloc_scratch();
    parent.emit(
        Op::Equal,
        vec![
            Operand::Register(cond_reg),
            Operand::Register(value_reg),
            Operand::Register(undef_reg),
        ],
        span,
    );
    // If the slot is **not** undefined, skip the default
    // evaluation entirely so the user's expression doesn't fire on
    // the common path.
    let skip_default = parent.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span);
    let default_value = compile_expr(parent, default_expr, span)?;
    parent.emit(
        Op::StoreLocal,
        vec![
            Operand::Register(default_value),
            Operand::Imm32(value_reg as i32),
        ],
        span,
    );
    parent.patch_branch_to_here(skip_default);
    Ok(())
}

/// Recursively destructure the value in `src_reg` into the named
/// bindings declared by `pattern`. Handles `BindingIdentifier`
/// (the leaf), `ArrayPattern` (via the iterator protocol),
/// `ObjectPattern` (via property loads with rename / default
/// support), and inner `AssignmentPattern` defaults.
fn destructure_into(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    destructure_pattern(parent, src_reg, pattern, span, false)
}

/// Mirror of [`destructure_into`] for `var` destructuring heads —
/// each leaf identifier resolves to an *existing* binding (the
/// var-hoist pass populated it at function entry) and is stored
/// rather than re-declared. Used by `for (var [a, b] of …)` etc.
fn destructure_assign(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    destructure_pattern(parent, src_reg, pattern, span, true)
}

fn destructure_pattern(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::BindingPattern<'_>,
    span: (u32, u32),
    assign_existing: bool,
) -> Result<(), CompileError> {
    match pattern {
        oxc_ast::ast::BindingPattern::BindingIdentifier(id) => {
            let name = id.name.as_str();
            if assign_existing {
                store_identifier(parent, name, src_reg, span)
            } else {
                let storage = parent.declare_binding(name, false, span)?;
                parent.emit_store_storage(src_reg, storage, span);
                parent.mark_initialized(name);
                Ok(())
            }
        }
        oxc_ast::ast::BindingPattern::AssignmentPattern(asgn) => {
            let asgn_span = (asgn.span.start, asgn.span.end);
            apply_default_into(parent, src_reg, &asgn.right, asgn_span)?;
            destructure_pattern(parent, src_reg, &asgn.left, span, assign_existing)
        }
        oxc_ast::ast::BindingPattern::ArrayPattern(arr) => {
            destructure_array_inner(parent, src_reg, arr, span, assign_existing)
        }
        oxc_ast::ast::BindingPattern::ObjectPattern(obj) => {
            destructure_object_inner(parent, src_reg, obj, span, assign_existing)
        }
    }
}

fn destructure_array_inner(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::ArrayPattern<'_>,
    span: (u32, u32),
    assign_existing: bool,
) -> Result<(), CompileError> {
    let iter_reg = parent.alloc_scratch();
    parent.emit(
        Op::GetIterator,
        vec![Operand::Register(iter_reg), Operand::Register(src_reg)],
        span,
    );
    for elem in &pattern.elements {
        let value_reg = parent.alloc_scratch();
        let done_reg = parent.alloc_scratch();
        parent.emit(
            Op::IteratorNext,
            vec![
                Operand::Register(value_reg),
                Operand::Register(done_reg),
                Operand::Register(iter_reg),
            ],
            span,
        );
        // A hole (`,,`) leaves the slot unbound — nothing to emit.
        let Some(inner) = elem else {
            continue;
        };
        destructure_pattern(parent, value_reg, inner, span, assign_existing)?;
    }
    if let Some(rest) = &pattern.rest {
        // Drain the rest of the iterator into a fresh array.
        let arr_reg = parent.alloc_scratch();
        parent.emit(
            Op::NewArray,
            vec![Operand::Register(arr_reg), Operand::ConstIndex(0)],
            span,
        );
        let value_reg = parent.alloc_scratch();
        let done_reg = parent.alloc_scratch();
        let loop_top = parent.next_pc;
        parent.emit(
            Op::IteratorNext,
            vec![
                Operand::Register(value_reg),
                Operand::Register(done_reg),
                Operand::Register(iter_reg),
            ],
            span,
        );
        let exit = parent.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
        parent.emit(
            Op::ArrayPush,
            vec![Operand::Register(arr_reg), Operand::Register(value_reg)],
            span,
        );
        let back = parent.emit_branch_placeholder(Op::Jump, None, span);
        parent.patch_branch(back, loop_top);
        parent.patch_branch_to_here(exit);
        destructure_pattern(parent, arr_reg, &rest.argument, span, assign_existing)?;
    }
    Ok(())
}

fn destructure_object_inner(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::ObjectPattern<'_>,
    span: (u32, u32),
    assign_existing: bool,
) -> Result<(), CompileError> {
    // Track keys extracted by named/computed properties so the
    // rest element (`...r`) can exclude them when copying the
    // remaining own enumerable properties.
    enum ExtractedKey {
        Static(String),
        Runtime(u16),
    }
    let mut extracted_keys: Vec<ExtractedKey> = Vec::new();

    for prop in &pattern.properties {
        let prop_span = (prop.span.start, prop.span.end);
        let value_reg = parent.alloc_scratch();
        if prop.computed {
            // §13.15.5 — computed key evaluated at destructuring
            // time, then `obj[key]` via `Op::LoadElement`.
            let key_reg = match &prop.key {
                oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                    let r = parent.alloc_scratch();
                    let s = parent.intern_string_constant(id.name.as_str());
                    parent.emit(
                        Op::LoadString,
                        vec![Operand::Register(r), Operand::ConstIndex(s)],
                        prop_span,
                    );
                    r
                }
                _ => compile_expr_as_property_key(parent, &prop.key, prop_span)?,
            };
            parent.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(value_reg),
                    Operand::Register(src_reg),
                    Operand::Register(key_reg),
                ],
                prop_span,
            );
            extracted_keys.push(ExtractedKey::Runtime(key_reg));
        } else {
            // Static identifier / string / numeric / bigint key —
            // resolved to a string at compile time.
            let key_str: Option<String> = match &prop.key {
                oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                    Some(id.name.as_str().to_string())
                }
                oxc_ast::ast::PropertyKey::StringLiteral(lit) => Some(lit.value.to_string()),
                oxc_ast::ast::PropertyKey::NumericLiteral(lit) => {
                    // §6.1.7.1 ToString(Number) — match runtime
                    // semantics so e.g. `1` and `1.0` both key as
                    // "1". Foundation defers to Rust f64 → string
                    // for the integer cases (NumericLiteral parses
                    // the source form).
                    Some(numeric_literal_to_property_key(lit.value))
                }
                oxc_ast::ast::PropertyKey::BigIntLiteral(lit) => {
                    // BigInt literal in property key: ToString
                    // strips the trailing `n`. oxc preserves the
                    // raw text including the suffix.
                    let raw = lit.raw.as_ref().map(|s| s.as_str()).unwrap_or("");
                    Some(raw.trim_end_matches('n').to_string())
                }
                _ => None,
            };
            match key_str {
                Some(s) => {
                    let key_const = parent.intern_string_constant(&s);
                    parent.emit(
                        Op::LoadProperty,
                        vec![
                            Operand::Register(value_reg),
                            Operand::Register(src_reg),
                            Operand::ConstIndex(key_const),
                        ],
                        prop_span,
                    );
                    extracted_keys.push(ExtractedKey::Static(s));
                }
                None => {
                    return Err(CompileError::Unsupported {
                        node: format!("ObjectPattern: non-string key ({:?})", prop.key),
                        span: prop_span,
                    });
                }
            }
        }
        destructure_pattern(parent, value_reg, &prop.value, prop_span, assign_existing)?;
    }

    if let Some(rest) = pattern.rest.as_ref() {
        // §13.15.5 RestObjectAssignment — build a fresh object,
        // copy every enumerable own property of `src`, then delete
        // each previously-extracted key.
        let rest_obj = parent.alloc_scratch();
        parent.emit(Op::NewObject, vec![Operand::Register(rest_obj)], span);
        let scratch = parent.alloc_scratch();
        parent.emit(
            Op::ObjectCall,
            vec![
                Operand::Register(scratch),
                Operand::ConstIndex(otter_bytecode::method_id::ObjectMethod::Assign.as_u32()),
                Operand::ConstIndex(2),
                Operand::Register(rest_obj),
                Operand::Register(src_reg),
            ],
            span,
        );
        for key in &extracted_keys {
            match key {
                ExtractedKey::Static(s) => {
                    let key_const = parent.intern_string_constant(s);
                    let del_dst = parent.alloc_scratch();
                    parent.emit(
                        Op::DeleteProperty,
                        vec![
                            Operand::Register(del_dst),
                            Operand::Register(rest_obj),
                            Operand::ConstIndex(key_const),
                        ],
                        span,
                    );
                }
                ExtractedKey::Runtime(key_reg) => {
                    let del_dst = parent.alloc_scratch();
                    parent.emit(
                        Op::DeleteElement,
                        vec![
                            Operand::Register(del_dst),
                            Operand::Register(rest_obj),
                            Operand::Register(*key_reg),
                        ],
                        span,
                    );
                }
            }
        }
        destructure_pattern(parent, rest_obj, &rest.argument, span, assign_existing)?;
    }
    Ok(())
}

/// Format a `NumericLiteral`'s value as a property key per
/// §6.1.7.1 ToString(Number). Integer values produce the bare
/// integer string ("1" not "1.0"); other finite numbers go
/// through Rust's default f64 formatter.
fn numeric_literal_to_property_key(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Lower a non-static `PropertyKey` to a register holding the
/// runtime key value. Used by destructuring patterns when the
/// key is a computed expression or a primitive literal that we
/// need at runtime (e.g. for delete in object-rest exclusion).
fn compile_expr_as_property_key(
    cx: &mut Compiler,
    key: &oxc_ast::ast::PropertyKey<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    use oxc_ast::ast::PropertyKey;
    if let Some(expr) = key.as_expression() {
        return compile_expr(cx, expr, span);
    }
    match key {
        PropertyKey::StaticIdentifier(id) => {
            let r = cx.alloc_scratch();
            let s = cx.intern_string_constant(id.name.as_str());
            cx.emit(
                Op::LoadString,
                vec![Operand::Register(r), Operand::ConstIndex(s)],
                span,
            );
            Ok(r)
        }
        PropertyKey::PrivateIdentifier(_) => Err(CompileError::Unsupported {
            node: "PrivateIdentifier as property key in pattern".to_string(),
            span,
        }),
        _ => Err(CompileError::Unsupported {
            node: format!("PropertyKey ({key:?}) in pattern"),
            span,
        }),
    }
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
fn compile_arrow_function(
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
        parent.emit(Op::ReturnValue, vec![Operand::Register(reg)], inner_span);
    } else {
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
fn emit_make_callable(
    cx: &mut Compiler,
    dst: u16,
    function_const: u32,
    captures: &[u32],
    is_arrow: bool,
    span: (u32, u32),
) {
    if captures.is_empty() && !is_arrow {
        cx.emit(
            Op::MakeFunction,
            vec![Operand::Register(dst), Operand::ConstIndex(function_const)],
            span,
        );
        return;
    }
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + captures.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::ConstIndex(function_const));
    operands.push(Operand::ConstIndex(captures.len() as u32));
    for &parent_idx in captures {
        operands.push(Operand::Imm32(parent_idx as i32));
    }
    cx.emit(Op::MakeClosure, operands, span);
}

/// Adapter for the `for(...; ...; ...)` initializer's
/// `Expression`-shaped variant. OXC's `ForStatementInit` is a
/// closed enum that mirrors `Expression`; this helper widens it
/// back to `&Expression` so the compiler can reuse `compile_expr`.
fn init_to_expression<'a, 'b>(
    init: &'a oxc_ast::ast::ForStatementInit<'b>,
) -> Option<&'a Expression<'b>> {
    init.as_expression()
}

fn compile_expr(
    cx: &mut Compiler,
    expr: &Expression<'_>,
    enclosing_span: (u32, u32),
) -> Result<u16, CompileError> {
    let expr = unwrap_ts_expr(expr);
    match expr {
        Expression::Identifier(id) if id.name.as_str() == "undefined" => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadUndefined,
                vec![Operand::Register(dst)],
                enclosing_span,
            );
            Ok(dst)
        }

        // §19.1 `globalThis` — when the user hasn't shadowed the
        // name, return the runtime's per-Interpreter shared
        // globalThis JsObject.
        // <https://tc39.es/ecma262/#sec-globalthis>
        Expression::Identifier(id)
            if id.name.as_str() == "globalThis"
                && cx.lookup_binding("globalThis").is_none()
                && find_module_import_binding(cx, "globalThis").is_none() =>
        {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadGlobalThis,
                vec![Operand::Register(dst)],
                enclosing_span,
            );
            Ok(dst)
        }

        Expression::NullLiteral(lit) => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadNull,
                vec![Operand::Register(dst)],
                (lit.span.start, lit.span.end),
            );
            Ok(dst)
        }

        Expression::ThisExpression(t) => {
            let span = (t.span.start, t.span.end);
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadThis, vec![Operand::Register(dst)], span);
            Ok(dst)
        }

        Expression::Super(s) => {
            // Bare `super` standalone is a SyntaxError in real JS;
            // the grammar only accepts it as a call target or as
            // the object of a member expression. We surface a
            // friendly compile-time diagnostic so the rejection
            // happens at the right layer.
            Err(CompileError::Unsupported {
                node: "Super: bare `super` outside call or member expression".to_string(),
                span: (s.span.start, s.span.end),
            })
        }

        Expression::Identifier(id) => {
            let span = (id.span.start, id.span.end);
            // Foundation pseudo-globals before falling back to
            // local resolution.
            match id.name.as_str() {
                "NaN" => {
                    let dst = cx.alloc_scratch();
                    let const_idx = cx.intern_number_constant(f64::NAN);
                    cx.emit(
                        Op::LoadNumber,
                        vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                        span,
                    );
                    return Ok(dst);
                }
                "Infinity" => {
                    let dst = cx.alloc_scratch();
                    let const_idx = cx.intern_number_constant(f64::INFINITY);
                    cx.emit(
                        Op::LoadNumber,
                        vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                        span,
                    );
                    return Ok(dst);
                }
                _ => {}
            }
            // ECMA-262 §19.3 / §20.5 native error constructors
            // (`Error`, `TypeError`, `RangeError`, `SyntaxError`,
            // `ReferenceError`, `URIError`, `EvalError`). Bare
            // identifier reads — e.g. `e instanceof TypeError` —
            // lower to `Op::LoadBuiltinError` so the runtime hands
            // back the per-interpreter constructor object whose
            // `prototype` own property feeds `Op::Instanceof`.
            // Local bindings of the same name still take precedence
            // (checked below via `lookup_binding`), so user code
            // can shadow the global if it really needs to.
            //
            // <https://tc39.es/ecma262/#sec-error-objects>
            if cx.lookup_binding(id.name.as_str()).is_none()
                && find_module_import_binding(cx, id.name.as_str()).is_none()
                && is_builtin_error_class_name(id.name.as_str())
            {
                let dst = cx.alloc_scratch();
                let kind_idx = cx.intern_string_constant(id.name.as_str());
                cx.emit(
                    Op::LoadBuiltinError,
                    vec![Operand::Register(dst), Operand::ConstIndex(kind_idx)],
                    span,
                );
                return Ok(dst);
            }
            // Module-mode identifier resolution: imported aliases
            // resolve to a `LoadProperty` against the source
            // module's import-record (live binding — every read
            // observes the current export value).
            //
            // Inner functions that reference an imported alias
            // walk up the function-context stack to find the
            // matching record-upvalue, then capture it via the
            // standard `resolve_capture` cascade so the cell is
            // available in the inner frame's upvalues array.
            //
            // Spec: <https://tc39.es/ecma262/#sec-getidentifierreference>
            //       <https://tc39.es/ecma262/#sec-module-environment-records-getbindingvalue-n-s>
            if let Some((binding, synthetic)) = find_module_import_binding(cx, id.name.as_str()) {
                let resolved_uv = if cx.module_state.is_some() {
                    binding.record_uv_idx
                } else {
                    cx.resolve_capture(&synthetic)
                        .expect("synthetic import-record binding must resolve")
                };
                let record_dst = cx.alloc_scratch();
                cx.emit(
                    Op::LoadUpvalue,
                    vec![
                        Operand::Register(record_dst),
                        Operand::Imm32(resolved_uv as i32),
                    ],
                    span,
                );
                if binding.is_namespace {
                    return Ok(record_dst);
                }
                let dst = cx.alloc_scratch();
                cx.emit_load_property(dst, record_dst, &binding.source_name, span);
                return Ok(dst);
            }
            if let Some(info) = cx.lookup_binding(id.name.as_str()) {
                let dst = cx.alloc_scratch();
                if info.initialized {
                    cx.emit_load_storage(dst, info.storage, span);
                } else {
                    // Reading a `let` / `const` binding before its
                    // initializer ran — runtime raises
                    // `ReferenceError` via `Op::TdzError`.
                    let diag_idx = match info.storage {
                        BindingStorage::Register { reg } => reg,
                        BindingStorage::Upvalue { idx } => idx,
                    };
                    cx.emit(Op::TdzError, vec![Operand::Imm32(diag_idx as i32)], span);
                }
                return Ok(dst);
            }
            // Walk the parent chain for a closure capture.
            if let Some(uv_idx) = cx.resolve_capture(id.name.as_str()) {
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::LoadUpvalue,
                    vec![Operand::Register(dst), Operand::Imm32(uv_idx as i32)],
                    span,
                );
                return Ok(dst);
            }
            // §10.2.4.1 ResolveBinding + §10.2.4.5 GetValue
            // fallback — an unbound free identifier resolves
            // against the global environment record (foundation:
            // `globalThis`). When the global has no own property
            // under that name, the runtime throws a
            // `ReferenceError` per the spec.
            //
            // <https://tc39.es/ecma262/#sec-resolvebinding>
            // <https://tc39.es/ecma262/#sec-getvalue>
            let dst = cx.alloc_scratch();
            let name_idx = cx.intern_string_constant(id.name.as_str());
            cx.emit(
                Op::LoadGlobalOrThrow,
                vec![Operand::Register(dst), Operand::ConstIndex(name_idx)],
                span,
            );
            Ok(dst)
        }

        Expression::LogicalExpression(l) => {
            let span = (l.span.start, l.span.end);
            // Lower `a && b`, `a || b`, `a ?? b` with short-circuit
            // semantics. The result lands in a fresh register and
            // both branches store into the same slot.
            let left = compile_expr(cx, &l.left, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(left), Operand::Imm32(dst as i32)],
                span,
            );
            // Note: locals and scratch share the same register
            // window. We use STORE_LOCAL into the freshly-allocated
            // scratch index so the JUMP target reads back through
            // LOAD_LOCAL — preserves register liveness across the
            // branch without a phi.
            let short_circuit = match l.operator {
                LogicalOperator::And => {
                    cx.emit_branch_placeholder(Op::JumpIfFalse, Some(left), span)
                }
                LogicalOperator::Or => cx.emit_branch_placeholder(Op::JumpIfTrue, Some(left), span),
                LogicalOperator::Coalesce => {
                    // `a ?? b`: if `a` is **not** nullish, short-
                    // circuit. JumpIfNullish jumps when nullish, so
                    // we want the **inverse**: emit a normal branch
                    // into "evaluate b" path when nullish, and let
                    // fall-through skip past `b`. Implement via two
                    // jumps for clarity.
                    let to_b = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(left), span);
                    let skip = cx.emit_branch_placeholder(Op::Jump, None, span);
                    cx.patch_branch_to_here(to_b);
                    let right = compile_expr(cx, &l.right, span)?;
                    cx.emit(
                        Op::StoreLocal,
                        vec![Operand::Register(right), Operand::Imm32(dst as i32)],
                        span,
                    );
                    cx.patch_branch_to_here(skip);
                    return Ok({
                        let out = cx.alloc_scratch();
                        cx.emit(
                            Op::LoadLocal,
                            vec![Operand::Register(out), Operand::Imm32(dst as i32)],
                            span,
                        );
                        out
                    });
                }
            };
            // Falling here for `&&` / `||`: evaluate `right` and
            // store; patch short-circuit at end.
            let right = compile_expr(cx, &l.right, span)?;
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(right), Operand::Imm32(dst as i32)],
                span,
            );
            cx.patch_branch_to_here(short_circuit);
            let out = cx.alloc_scratch();
            cx.emit(
                Op::LoadLocal,
                vec![Operand::Register(out), Operand::Imm32(dst as i32)],
                span,
            );
            Ok(out)
        }

        Expression::ConditionalExpression(c) => {
            let span = (c.span.start, c.span.end);
            let cond = compile_expr(cx, &c.test, span)?;
            let dst = cx.alloc_scratch();
            let to_alt = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond), span);
            let cons = compile_expr(cx, &c.consequent, span)?;
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(cons), Operand::Imm32(dst as i32)],
                span,
            );
            let to_end = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch_to_here(to_alt);
            let alt = compile_expr(cx, &c.alternate, span)?;
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(alt), Operand::Imm32(dst as i32)],
                span,
            );
            cx.patch_branch_to_here(to_end);
            let out = cx.alloc_scratch();
            cx.emit(
                Op::LoadLocal,
                vec![Operand::Register(out), Operand::Imm32(dst as i32)],
                span,
            );
            Ok(out)
        }

        Expression::AssignmentExpression(a) => compile_assignment(cx, a),

        Expression::StringLiteral(lit) => {
            let dst = cx.alloc_scratch();
            let const_idx = if lit.lone_surrogates {
                let utf16 = decode_lone_surrogate_string(&lit.value);
                cx.intern_utf16_string_constant(utf16)
            } else {
                cx.intern_string_constant(&lit.value)
            };
            cx.emit(
                Op::LoadString,
                vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                (lit.span.start, lit.span.end),
            );
            Ok(dst)
        }

        Expression::BigIntLiteral(lit) => {
            let span = (lit.span.start, lit.span.end);
            let dst = cx.alloc_scratch();
            let decimal = lit.value.as_str().to_string();
            // Compile-time syntactic validation so the runtime
            // parse path can stay strict (treats failure as
            // `InvalidOperand` rather than a surfaced parse error).
            if decimal.parse::<num_bigint::BigInt>().is_err() {
                return Err(CompileError::Unsupported {
                    node: format!("BigIntLiteral with non-decimal payload `{decimal}`"),
                    span,
                });
            }
            let const_idx = cx.intern_bigint_constant(&decimal);
            cx.emit(
                Op::LoadBigInt,
                vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                span,
            );
            Ok(dst)
        }

        Expression::RegExpLiteral(lit) => {
            let span = (lit.span.start, lit.span.end);
            let pattern_text = lit.regex.pattern.text.as_str();
            let flags_str = lit.regex.flags.to_string();
            // Compile-time validation: feed the pattern + flags to
            // `regress` so we surface a clean `Unsupported` for the
            // few patterns the engine rejects (e.g. unterminated
            // groups). Mirrors the BigIntLiteral approach. The `g`,
            // `y`, and `d` flags live above the matcher per JS spec
            // (§22.2.6.4 [`get RegExp.prototype.flags`](https://tc39.es/ecma262/#sec-get-regexp.prototype.flags)),
            // so we strip them before asking `regress` to compile.
            let mut engine_flags = regress::Flags::default();
            let mut saw_u = false;
            let mut saw_v = false;
            for c in flags_str.chars() {
                match c {
                    'd' | 'g' | 'y' => {}
                    'i' => engine_flags.icase = true,
                    'm' => engine_flags.multiline = true,
                    's' => engine_flags.dot_all = true,
                    'u' => {
                        engine_flags.unicode = true;
                        saw_u = true;
                    }
                    'v' => {
                        engine_flags.unicode_sets = true;
                        saw_v = true;
                    }
                    other => {
                        return Err(CompileError::Unsupported {
                            node: format!(
                                "RegExpLiteral `/{pattern_text}/{flags_str}` has unsupported flag `{other}`"
                            ),
                            span,
                        });
                    }
                }
            }
            if saw_u && saw_v {
                return Err(CompileError::Unsupported {
                    node: format!(
                        "RegExpLiteral `/{pattern_text}/{flags_str}` rejected: flags `u` and `v` are mutually exclusive (§22.2.4)"
                    ),
                    span,
                });
            }
            if let Err(e) = regress::Regex::with_flags(pattern_text, engine_flags) {
                return Err(CompileError::Unsupported {
                    node: format!("RegExpLiteral `/{pattern_text}/{flags_str}` rejected: {e}"),
                    span,
                });
            }
            let pattern_utf16: Vec<u16> = pattern_text.encode_utf16().collect();
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_regexp_constant(&pattern_utf16, &flags_str);
            cx.emit(
                Op::LoadRegExp,
                vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                span,
            );
            Ok(dst)
        }

        Expression::NumericLiteral(lit) => {
            let dst = cx.alloc_scratch();
            let span = (lit.span.start, lit.span.end);
            // Smi fast path: integer-valued literal in i32 range.
            if lit.value.fract() == 0.0
                && lit.value.is_finite()
                && (i32::MIN as f64..=i32::MAX as f64).contains(&lit.value)
                && !(lit.value == 0.0 && lit.value.is_sign_negative())
            {
                cx.emit(
                    Op::LoadInt32,
                    vec![Operand::Register(dst), Operand::Imm32(lit.value as i32)],
                    span,
                );
            } else {
                let const_idx = cx.intern_number_constant(lit.value);
                cx.emit(
                    Op::LoadNumber,
                    vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                    span,
                );
            }
            Ok(dst)
        }

        Expression::BooleanLiteral(lit) => {
            let dst = cx.alloc_scratch();
            let span = (lit.span.start, lit.span.end);
            cx.emit(
                if lit.value {
                    Op::LoadTrue
                } else {
                    Op::LoadFalse
                },
                vec![Operand::Register(dst)],
                span,
            );
            Ok(dst)
        }

        Expression::UnaryExpression(u) => {
            let span = (u.span.start, u.span.end);
            // `delete obj.prop` is special: the operand isn't a
            // value-producing expression, it's a member reference.
            if matches!(u.operator, UnaryOperator::Delete) {
                if cx.is_strict
                    && let Expression::Identifier(id) = &u.argument
                {
                    return Err(CompileError::Unsupported {
                        node: format!("strict delete of identifier `{}`", id.name.as_str()),
                        span,
                    });
                }
                if let Expression::StaticMemberExpression(member) = &u.argument {
                    let obj_reg = compile_expr(cx, &member.object, span)?;
                    let name_idx = cx.intern_string_constant(member.property.name.as_str());
                    let dst = cx.alloc_scratch();
                    cx.emit(
                        Op::DeleteProperty,
                        vec![
                            Operand::Register(dst),
                            Operand::Register(obj_reg),
                            Operand::ConstIndex(name_idx),
                        ],
                        span,
                    );
                    return Ok(dst);
                }
                if let Expression::ComputedMemberExpression(member) = &u.argument {
                    let obj_reg = compile_expr(cx, &member.object, span)?;
                    let idx_reg = compile_expr(cx, &member.expression, span)?;
                    let dst = cx.alloc_scratch();
                    cx.emit(
                        Op::DeleteElement,
                        vec![
                            Operand::Register(dst),
                            Operand::Register(obj_reg),
                            Operand::Register(idx_reg),
                        ],
                        span,
                    );
                    return Ok(dst);
                }
                // §13.5.1.2 — `delete` on a non-Reference returns
                // `true`. The argument is still evaluated for side
                // effects, then we discard it.
                // <https://tc39.es/ecma262/#sec-delete-operator-runtime-semantics-evaluation>
                let _ = compile_expr(cx, &u.argument, span)?;
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadTrue, vec![Operand::Register(dst)], span);
                return Ok(dst);
            }
            // §13.5.2 `void expr` — evaluate, discard, return `undefined`.
            // <https://tc39.es/ecma262/#sec-void-operator>
            if matches!(u.operator, UnaryOperator::Void) {
                let _ = compile_expr(cx, &u.argument, span)?;
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
                return Ok(dst);
            }
            // §13.5.3 `typeof Identifier` — IsUnresolvableReference
            // returns `"undefined"` rather than throwing
            // ReferenceError. Re-route the global-fallback step to
            // `LoadGlobalOrUndefined` so an unbound free identifier
            // never throws under `typeof`.
            // <https://tc39.es/ecma262/#sec-typeof-operator>
            if matches!(u.operator, UnaryOperator::Typeof)
                && let Expression::Identifier(id) = &u.argument
            {
                let name = id.name.as_str();
                if cx.lookup_binding(name).is_none()
                    && find_module_import_binding(cx, name).is_none()
                    && cx.resolve_capture(name).is_none()
                    && !is_builtin_error_class_name(name)
                    && name != "NaN"
                    && name != "Infinity"
                    && name != "undefined"
                {
                    let value_reg = cx.alloc_scratch();
                    let name_idx = cx.intern_string_constant(name);
                    cx.emit(
                        Op::LoadGlobalOrUndefined,
                        vec![Operand::Register(value_reg), Operand::ConstIndex(name_idx)],
                        span,
                    );
                    let dst = cx.alloc_scratch();
                    cx.emit(
                        Op::TypeOf,
                        vec![Operand::Register(dst), Operand::Register(value_reg)],
                        span,
                    );
                    return Ok(dst);
                }
            }
            let inner = compile_expr(cx, &u.argument, span)?;
            let dst = cx.alloc_scratch();
            let op = match u.operator {
                UnaryOperator::UnaryNegation => Op::Neg,
                UnaryOperator::UnaryPlus => Op::ToNumber,
                UnaryOperator::LogicalNot => Op::LogicalNot,
                UnaryOperator::BitwiseNot => Op::BitwiseNot,
                UnaryOperator::Typeof => Op::TypeOf,
                other => {
                    return Err(CompileError::Unsupported {
                        node: format!("UnaryExpression ({other:?})"),
                        span,
                    });
                }
            };
            // §13.5.4–13.5.7 — unary `+`, `-`, `~` apply
            // ToPrimitive(number) before ToNumeric so an object
            // operand goes through `[Symbol.toPrimitive]` /
            // `valueOf` / `toString`. LogicalNot and TypeOf do
            // not coerce; they take their argument as-is.
            // <https://tc39.es/ecma262/#sec-unary-operators>
            let inner_in = match op {
                Op::Neg | Op::ToNumber | Op::BitwiseNot => {
                    emit_to_primitive(cx, inner, "number", span)
                }
                _ => inner,
            };
            cx.emit(
                op,
                vec![Operand::Register(dst), Operand::Register(inner_in)],
                span,
            );
            Ok(dst)
        }

        // §13.16 — `(a, b, c)`. Evaluate each in order, return the
        // last value.
        // <https://tc39.es/ecma262/#sec-comma-operator>
        Expression::SequenceExpression(s) => {
            let span = (s.span.start, s.span.end);
            let mut last = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, vec![Operand::Register(last)], span);
            for expr in s.expressions.iter() {
                last = compile_expr(cx, expr, span)?;
            }
            Ok(last)
        }

        Expression::TemplateLiteral(t) => compile_template_literal(cx, t),

        // §13.3.11 TaggedTemplate — `tag` call with `(strings, ...exprs)`.
        // <https://tc39.es/ecma262/#sec-tagged-templates>
        Expression::TaggedTemplateExpression(t) => compile_tagged_template(cx, t),

        // §13.3.9 Optional Chaining (`a?.b`, `a?.[k]`, `a?.()`).
        // <https://tc39.es/ecma262/#sec-optional-chains>
        Expression::ChainExpression(c) => compile_chain_expression(cx, c),

        // §13.3.7 PrivateFieldExpression — `obj.#name`.
        // <https://tc39.es/ecma262/#sec-makeprivatereference>
        Expression::PrivateFieldExpression(p) => {
            let pspan = (p.span.start, p.span.end);
            let mangled =
                cx.mangle_private(p.field.name.as_str())
                    .ok_or(CompileError::Unsupported {
                        node: "PrivateFieldExpression outside any class body".to_string(),
                        span: pspan,
                    })?;
            let obj_reg = compile_expr(cx, &p.object, pspan)?;
            let name_idx = cx.intern_string_constant(&mangled);
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(dst),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                pspan,
            );
            Ok(dst)
        }

        // §13.10.1 — `#name in obj` private-name membership probe.
        // <https://tc39.es/ecma262/#sec-relational-operators-runtime-semantics-evaluation>
        Expression::PrivateInExpression(p) => {
            let pspan = (p.span.start, p.span.end);
            let mangled =
                cx.mangle_private(p.left.name.as_str())
                    .ok_or(CompileError::Unsupported {
                        node: "PrivateInExpression outside any class body".to_string(),
                        span: pspan,
                    })?;
            let key_reg = cx.alloc_scratch();
            let key_idx = cx.intern_string_constant(&mangled);
            cx.emit(
                Op::LoadString,
                vec![Operand::Register(key_reg), Operand::ConstIndex(key_idx)],
                pspan,
            );
            let obj_reg = compile_expr(cx, &p.right, pspan)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::HasProperty,
                vec![
                    Operand::Register(dst),
                    Operand::Register(key_reg),
                    Operand::Register(obj_reg),
                ],
                pspan,
            );
            Ok(dst)
        }

        Expression::BinaryExpression(b) => {
            let span = (b.span.start, b.span.end);
            let lhs = compile_expr(cx, &b.left, span)?;
            let rhs = compile_expr(cx, &b.right, span)?;
            let op = match b.operator {
                BinaryOperator::Addition => Op::Add,
                BinaryOperator::Subtraction => Op::Sub,
                BinaryOperator::Multiplication => Op::Mul,
                BinaryOperator::Division => Op::Div,
                BinaryOperator::Remainder => Op::Rem,
                BinaryOperator::Exponential => Op::Pow,
                BinaryOperator::BitwiseAnd => Op::BitwiseAnd,
                BinaryOperator::BitwiseOR => Op::BitwiseOr,
                BinaryOperator::BitwiseXOR => Op::BitwiseXor,
                BinaryOperator::ShiftLeft => Op::Shl,
                BinaryOperator::ShiftRight => Op::Shr,
                BinaryOperator::ShiftRightZeroFill => Op::Ushr,
                BinaryOperator::StrictEquality => Op::Equal,
                BinaryOperator::StrictInequality => Op::NotEqual,
                // §7.2.13 IsLooselyEqual — operands flow through
                // `Op::ToPrimitive(default)` below before the
                // runtime applies the type-coercion table.
                BinaryOperator::Equality => Op::LooseEqual,
                BinaryOperator::Inequality => Op::LooseNotEqual,
                BinaryOperator::LessThan => Op::LessThan,
                BinaryOperator::LessEqualThan => Op::LessEq,
                BinaryOperator::GreaterThan => Op::GreaterThan,
                BinaryOperator::GreaterEqualThan => Op::GreaterEq,
                BinaryOperator::Instanceof => Op::Instanceof,
                // §13.10.1 `RelationalExpression in ShiftExpression`.
                // <https://tc39.es/ecma262/#sec-relational-operators-runtime-semantics-evaluation>
                BinaryOperator::In => Op::HasProperty,
            };
            // §13.15.4 ApplyStringOrNumericBinaryOperator step 1
            // requires both operands of `+` to pass through
            // `ToPrimitive(default)` before the runtime decides
            // between string concat and numeric add. Emit that
            // coercion at compile time so the runtime never sees
            // a non-primitive operand on the `Op::Add` fast path.
            //
            // §7.2.13 `IsLooselyEqual` (`==` / `!=`) consults
            // `[Symbol.toPrimitive]` on object operands too. Same
            // shape — emit `ToPrimitive(default)` and let the
            // runtime work over primitives.
            //
            // §7.2.14 `AbstractRelationalComparison` (`<`, `<=`,
            // `>`, `>=`) consults `ToPrimitive(number)` on each
            // operand per step 1.
            //
            // <https://tc39.es/ecma262/#sec-applystringornumericbinaryoperator>
            // <https://tc39.es/ecma262/#sec-islooselyequal>
            // <https://tc39.es/ecma262/#sec-abstract-relational-comparison>
            let (lhs_in, rhs_in) = match op {
                Op::Add | Op::LooseEqual | Op::LooseNotEqual => {
                    let l = emit_to_primitive(cx, lhs, "default", span);
                    let r = emit_to_primitive(cx, rhs, "default", span);
                    (l, r)
                }
                Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
                    let l = emit_to_primitive(cx, lhs, "number", span);
                    let r = emit_to_primitive(cx, rhs, "number", span);
                    (l, r)
                }
                // §13.15.3 ApplyStringOrNumericBinaryOperator —
                // non-additive numeric and bitwise/shift ops apply
                // ToPrimitive(number) to each operand before
                // ToNumeric. Pre-coerce here so the runtime never
                // sees a non-primitive operand.
                Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::Pow
                | Op::BitwiseAnd
                | Op::BitwiseOr
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr => {
                    let l = emit_to_primitive(cx, lhs, "number", span);
                    let r = emit_to_primitive(cx, rhs, "number", span);
                    (l, r)
                }
                _ => (lhs, rhs),
            };
            let dst = cx.alloc_scratch();
            cx.emit(
                op,
                vec![
                    Operand::Register(dst),
                    Operand::Register(lhs_in),
                    Operand::Register(rhs_in),
                ],
                span,
            );
            Ok(dst)
        }

        Expression::StaticMemberExpression(m) => {
            // General named member access. The runtime resolves
            // `string.length` as the special-case length getter and
            // walks `JsObject` properties for objects.
            let span = (m.span.start, m.span.end);
            // `super.x` reads the parent prototype's property — the
            // runtime walks one hop up `__class_home`'s prototype
            // chain. Only valid inside a class method.
            if matches!(m.object, Expression::Super(_)) {
                return compile_super_member_load(cx, m.property.name.as_str(), span);
            }
            // §23.2.5 TypedArray-constructor static properties:
            // `<T>.BYTES_PER_ELEMENT`. Lower the integer value at
            // compile time so the runtime does not need a real
            // constructor object.
            // <https://tc39.es/ecma262/#sec-typedarray.bytes_per_element>
            if let Expression::Identifier(id) = &m.object
                && is_typed_array_name(id.name.as_str())
                && m.property.name.as_str() == "BYTES_PER_ELEMENT"
                && cx.lookup_binding(id.name.as_str()).is_none()
                && find_module_import_binding(cx, id.name.as_str()).is_none()
            {
                let bpe: i32 = match id.name.as_str() {
                    "Int8Array" | "Uint8Array" | "Uint8ClampedArray" => 1,
                    "Int16Array" | "Uint16Array" => 2,
                    "Int32Array" | "Uint32Array" | "Float32Array" => 4,
                    _ => 8,
                };
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::LoadInt32,
                    vec![Operand::Register(dst), Operand::Imm32(bpe)],
                    span,
                );
                return Ok(dst);
            }
            // §21.1.1.x Number static constants — `MAX_SAFE_INTEGER`
            // / `MIN_SAFE_INTEGER` / `MAX_VALUE` / `MIN_VALUE` /
            // `EPSILON` / `POSITIVE_INFINITY` / `NEGATIVE_INFINITY`
            // / `NaN`. Inline the literal value at compile time so
            // the runtime doesn't need a real `Number` global.
            // <https://tc39.es/ecma262/#sec-properties-of-the-number-constructor>
            if let Expression::Identifier(id) = &m.object
                && id.name.as_str() == "Number"
                && cx.lookup_binding("Number").is_none()
                && find_module_import_binding(cx, "Number").is_none()
                && let Some(value) = number_static_constant(m.property.name.as_str())
            {
                let dst = cx.alloc_scratch();
                let const_idx = cx.intern_number_constant(value);
                cx.emit(
                    Op::LoadNumber,
                    vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                    span,
                );
                return Ok(dst);
            }
            // `Math.PI` / `Math.E` / other value properties lower to
            // MathLoad. Method reads fall through to ordinary property
            // load now that task 96 installs a real `Math` namespace.
            if let Expression::Identifier(id) = &m.object
                && id.name.as_str() == "Math"
                && math_static_constant(m.property.name.as_str()).is_some()
            {
                let dst = cx.alloc_scratch();
                let name_idx = cx.intern_string_constant(m.property.name.as_str());
                cx.emit(
                    Op::MathLoad,
                    vec![Operand::Register(dst), Operand::ConstIndex(name_idx)],
                    span,
                );
                return Ok(dst);
            }
            // `Symbol.<name>` — well-known symbol read. The runtime
            // resolves the name against the per-interpreter
            // well-known table (ECMA-262 §6.1.5.1).
            if let Expression::Identifier(id) = &m.object
                && id.name.as_str() == "Symbol"
            {
                let dst = cx.alloc_scratch();
                let name_idx = cx.intern_string_constant(m.property.name.as_str());
                cx.emit(
                    Op::SymbolLoad,
                    vec![Operand::Register(dst), Operand::ConstIndex(name_idx)],
                    span,
                );
                return Ok(dst);
            }
            let receiver = compile_expr(cx, &m.object, span)?;
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(dst),
                    Operand::Register(receiver),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            Ok(dst)
        }

        // `s[i]` — runtime checks that `s` is a string.
        Expression::ComputedMemberExpression(m) => {
            let span = (m.span.start, m.span.end);
            let recv = compile_expr(cx, &m.object, span)?;
            let idx = compile_expr(cx, &m.expression, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(dst),
                    Operand::Register(recv),
                    Operand::Register(idx),
                ],
                span,
            );
            Ok(dst)
        }

        // `recv.method(arg0, arg1, ...)` — dispatched through the
        // String.prototype intrinsic table at run time.
        Expression::CallExpression(call) => compile_method_call(cx, call),

        // `new Callee(args...)` — emits `Op::New`. The runtime
        // allocates the receiver and links its prototype. The
        // built-in `Error` constructor keeps a fast lowering path
        // since it doesn't need a `prototype` chain to work.
        Expression::NewExpression(new_expr) => {
            let new_span = (new_expr.span.start, new_expr.span.end);
            let callee = unwrap_ts_expr(&new_expr.callee);
            // ECMA-262 §19.3 / §20.5 native error constructors —
            // every one of `Error`, `TypeError`, `RangeError`,
            // `SyntaxError`, `ReferenceError`, `URIError`,
            // `EvalError` lowers to a dedicated opcode that
            // consults the per-interpreter [`ErrorClassRegistry`]
            // for the right prototype linkage.
            //
            // <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
            if let Expression::Identifier(id) = callee
                && cx.lookup_binding(id.name.as_str()).is_none()
                && find_module_import_binding(cx, id.name.as_str()).is_none()
                && is_builtin_error_class_name(id.name.as_str())
                && builtin_error_construct_fast_path_applies(id.name.as_str(), &new_expr.arguments)
            {
                return compile_builtin_error_construct(
                    cx,
                    id.name.as_str(),
                    &new_expr.arguments,
                    new_span,
                );
            }
            // §20.1.1 `new Object()` / `new Object(value)` — bare-
            // call form lowered to a fresh object via `Op::NewObject`
            // when no args, or pass-through for object-typed args.
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "Object"
                && cx.lookup_binding("Object").is_none()
                && find_module_import_binding(cx, "Object").is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                if arg_regs.is_empty() {
                    let dst = cx.alloc_scratch();
                    cx.emit(Op::NewObject, vec![Operand::Register(dst)], new_span);
                    return Ok(dst);
                }
                return Ok(arg_regs[0]);
            }
            // §23.1.1.1 `new Array(...)` — typed
            // [`Op::ArrayConstruct`]. Single-numeric form reserves
            // a sparse array of that length; everything else
            // collects values like `Array.of`.
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "Array"
                && cx.lookup_binding("Array").is_none()
                && find_module_import_binding(cx, "Array").is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::ArrayConstruct, operands, new_span);
                return Ok(dst);
            }
            // §22.1.1 `new String(value)` falls through to the
            // general constructor path so runtime bootstrap can
            // produce a String wrapper object with [[StringData]].
            // §21.1.1 `new Number(value)` no longer aliases here —
            // the `Number` global is now a real `ClassConstructor`
            // (see `bootstrap::install_number`) and the construct
            // form must produce a `NumberObject` wrapper with the
            // `[[NumberData]]` slot set, not a primitive Number.
            // Falls through to the general `NewExpression` path.
            // §20.3.1 `new Boolean(value)` falls through to the
            // general constructor path so runtime bootstrap can
            // produce a Boolean wrapper object with [[BooleanData]].
            // §25.2.1 `new SharedArrayBuffer(length [, options])`.
            // Lowers via [`SharedArrayBufferMethod::Construct`].
            // <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "SharedArrayBuffer"
                && cx.lookup_binding("SharedArrayBuffer").is_none()
                && find_module_import_binding(cx, "SharedArrayBuffer").is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(
                    otter_bytecode::method_id::SharedArrayBufferMethod::Construct.as_u32(),
                ));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::SharedArrayBufferCall, operands, new_span);
                return Ok(dst);
            }
            // §25.1.4 `new ArrayBuffer(length [, options])`.
            // Lowers via [`ArrayBufferMethod::Construct`].
            // <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "ArrayBuffer"
                && cx.lookup_binding("ArrayBuffer").is_none()
                && find_module_import_binding(cx, "ArrayBuffer").is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(
                    otter_bytecode::method_id::ArrayBufferMethod::Construct.as_u32(),
                ));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::ArrayBufferCall, operands, new_span);
                return Ok(dst);
            }
            // §25.3.1 `new DataView(buffer, byteOffset?, byteLength?)`.
            // Lowers via [`DataViewMethod::Construct`].
            // <https://tc39.es/ecma262/#sec-dataview-constructor>
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "DataView"
                && cx.lookup_binding("DataView").is_none()
                && find_module_import_binding(cx, "DataView").is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(
                    otter_bytecode::method_id::DataViewMethod::Construct.as_u32(),
                ));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::DataViewCall, operands, new_span);
                return Ok(dst);
            }
            // §23.2.5 `new <T>(...)` for one of the eleven concrete
            // TypedArray constructors. Encodes the kind discriminant
            // and [`TypedArrayMethod::Construct`] directly.
            // <https://tc39.es/ecma262/#sec-typedarray-constructors>
            if let Expression::Identifier(id) = callee
                && let Some(kind) =
                    otter_bytecode::method_id::TypedArrayKindId::from_str(id.name.as_str())
                && cx.lookup_binding(id.name.as_str()).is_none()
                && find_module_import_binding(cx, id.name.as_str()).is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(kind.as_u32()));
                operands.push(Operand::ConstIndex(
                    otter_bytecode::method_id::TypedArrayMethod::Construct.as_u32(),
                ));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::TypedArrayCall, operands, new_span);
                return Ok(dst);
            }
            // §21.4.2 `new Date(...)` — variadic constructor.
            // Lowers via [`DateMethod::Construct`].
            // <https://tc39.es/ecma262/#sec-date-constructor>
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "Date"
                && cx.lookup_binding("Date").is_none()
                && find_module_import_binding(cx, "Date").is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(
                    otter_bytecode::method_id::DateMethod::Construct.as_u32(),
                ));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::DateCall, operands, new_span);
                return Ok(dst);
            }
            // §20.2.1.1 `new Function(arg0, …, body)` — every
            // argument coerces to a string at runtime; the leading
            // ones become parameter names and the last one is the
            // function body. Foundation lowers `Function(...)`
            // (without `new`) to the same shape per spec.
            // <https://tc39.es/ecma262/#sec-function-p1-p2-pn-body>
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "Function"
                && cx.lookup_binding("Function").is_none()
                && find_module_import_binding(cx, "Function").is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::NewFunction, operands, new_span);
                return Ok(dst);
            }
            // `new Intl.<Class>(locale?, options?)` — dedicated
            // `Op::NewIntl` lowering. The callee is a static-member
            // expression `Intl.<Class>`; we pull the class name out
            // of the property and emit the constructor opcode.
            if let Expression::StaticMemberExpression(member) = callee
                && let Expression::Identifier(id) = &member.object
                && id.name.as_str() == "Intl"
                && matches!(
                    member.property.name.as_str(),
                    "Collator"
                        | "NumberFormat"
                        | "DateTimeFormat"
                        | "PluralRules"
                        | "RelativeTimeFormat"
                        | "ListFormat"
                        | "DisplayNames"
                        | "Segmenter"
                )
            {
                let class = member.property.name.as_str();
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let locale_reg = arg_regs.first().copied().unwrap_or_else(|| {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], new_span);
                    r
                });
                let options_reg = arg_regs.get(1).copied().unwrap_or_else(|| {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], new_span);
                    r
                });
                let dst = cx.alloc_scratch();
                let class_idx = cx.intern_string_constant(class);
                cx.emit(
                    Op::NewIntl,
                    vec![
                        Operand::Register(dst),
                        Operand::ConstIndex(class_idx),
                        Operand::Register(locale_reg),
                        Operand::Register(options_reg),
                    ],
                    new_span,
                );
                return Ok(dst);
            }
            // `new Map(iter?)` / `new Set(iter?)` /
            // `new WeakMap(iter?)` / `new WeakSet(iter?)` —
            // dedicated `Op::NewCollection` lowering. Iterable
            // argument is optional; when omitted the collection is
            // empty.
            if let Expression::Identifier(id) = callee
                && matches!(id.name.as_str(), "Map" | "Set" | "WeakMap" | "WeakSet")
            {
                let kind = id.name.as_str();
                let iter_reg = if new_expr.arguments.is_empty() {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], new_span);
                    r
                } else {
                    let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                    arg_regs[0]
                };
                let dst = cx.alloc_scratch();
                let kind_idx = cx.intern_string_constant(kind);
                cx.emit(
                    Op::NewCollection,
                    vec![
                        Operand::Register(dst),
                        Operand::ConstIndex(kind_idx),
                        Operand::Register(iter_reg),
                    ],
                    new_span,
                );
                return Ok(dst);
            }
            // `new WeakRef(target)` / `new FinalizationRegistry(cb)`.
            if let Expression::Identifier(id) = callee
                && matches!(id.name.as_str(), "WeakRef" | "FinalizationRegistry")
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let first_arg = arg_regs.first().copied().unwrap_or_else(|| {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], new_span);
                    r
                });
                let dst = cx.alloc_scratch();
                let op = if id.name.as_str() == "WeakRef" {
                    Op::NewWeakRef
                } else {
                    Op::NewFinalizationRegistry
                };
                cx.emit(
                    op,
                    vec![Operand::Register(dst), Operand::Register(first_arg)],
                    new_span,
                );
                return Ok(dst);
            }
            // §28.2.1 `new Proxy(target, handler)` — lower via
            // [`ProxyMethod::Construct`].
            // <https://tc39.es/ecma262/#sec-proxy-constructor>
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "Proxy"
                && cx.lookup_binding("Proxy").is_none()
                && find_module_import_binding(cx, "Proxy").is_none()
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(
                    otter_bytecode::method_id::ProxyMethod::Construct.as_u32(),
                ));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::ProxyCall, operands, new_span);
                return Ok(dst);
            }
            // `new Promise(executor)` lowers to a dedicated
            // opcode that builds a pending promise + native
            // resolve/reject + invokes the executor.
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "Promise"
            {
                let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
                if arg_regs.len() != 1 {
                    return Err(CompileError::Unsupported {
                        node: "Promise constructor requires exactly one executor argument"
                            .to_string(),
                        span: new_span,
                    });
                }
                let executor_reg = arg_regs[0];
                let dst = cx.alloc_scratch();
                let scratch = cx.alloc_scratch();
                cx.emit(
                    Op::PromiseNew,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(executor_reg),
                        Operand::Register(scratch),
                    ],
                    new_span,
                );
                return Ok(dst);
            }
            // §13.3.5 NewExpression — `new C(...args)` may include
            // SpreadElement arguments. Route those through
            // `Op::NewSpread` (mirrors `Op::CallSpread` for calls).
            let has_spread = new_expr
                .arguments
                .iter()
                .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
            let callee_reg = compile_expr(cx, callee, new_span)?;
            if has_spread {
                let args_reg = compile_spread_call_args(cx, &new_expr.arguments, new_span)?;
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::NewSpread,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(callee_reg),
                        Operand::Register(args_reg),
                    ],
                    new_span,
                );
                return Ok(dst);
            }
            let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::New, operands, new_span);
            Ok(dst)
        }

        Expression::ParenthesizedExpression(p) => {
            compile_expr(cx, &p.expression, (p.span.start, p.span.end))
        }

        Expression::ArrayExpression(arr) => {
            let span = (arr.span.start, arr.span.end);
            let has_spread = arr
                .elements
                .iter()
                .any(|el| matches!(el, oxc_ast::ast::ArrayExpressionElement::SpreadElement(_)));
            if !has_spread {
                let mut element_regs: Vec<u16> = Vec::with_capacity(arr.elements.len());
                for el in &arr.elements {
                    match el {
                        oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => {
                            unreachable!("spread excluded above")
                        }
                        oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                            // §10.4.2 ArrayExoticObject: emit the
                            // internal hole sentinel so the resulting
                            // dense slot stays distinguishable from
                            // explicit `undefined` for `in`,
                            // `Array.prototype` callbacks, and JSON
                            // serialisation.
                            let r = cx.alloc_scratch();
                            cx.emit(Op::LoadHole, vec![Operand::Register(r)], span);
                            element_regs.push(r);
                        }
                        other => {
                            let expr = other.to_expression();
                            element_regs.push(compile_expr(cx, expr, span)?);
                        }
                    }
                }
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(2 + element_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(element_regs.len() as u32));
                operands.extend(element_regs.into_iter().map(Operand::Register));
                cx.emit(Op::NewArray, operands, span);
                Ok(dst)
            } else {
                // Spread path: materialise an empty array, then
                // append each element (or each iterator step for
                // spread elements). Slightly less efficient than
                // the dense `NewArray` form, but only paid for
                // literals that actually contain `...`.
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::NewArray,
                    vec![Operand::Register(dst), Operand::ConstIndex(0)],
                    span,
                );
                for el in &arr.elements {
                    match el {
                        oxc_ast::ast::ArrayExpressionElement::SpreadElement(s) => {
                            let inner_span = (s.span.start, s.span.end);
                            emit_spread_into_array(cx, dst, &s.argument, inner_span)?;
                        }
                        oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                            // Spread path's hole branch: same hole
                            // sentinel as the dense `NewArray` form
                            // above. `Op::ArrayPush` simply forwards
                            // the register value into the body.
                            let r = cx.alloc_scratch();
                            cx.emit(Op::LoadHole, vec![Operand::Register(r)], span);
                            cx.emit(
                                Op::ArrayPush,
                                vec![Operand::Register(dst), Operand::Register(r)],
                                span,
                            );
                        }
                        other => {
                            let expr = other.to_expression();
                            let r = compile_expr(cx, expr, span)?;
                            cx.emit(
                                Op::ArrayPush,
                                vec![Operand::Register(dst), Operand::Register(r)],
                                span,
                            );
                        }
                    }
                }
                Ok(dst)
            }
        }

        Expression::ObjectExpression(obj) => {
            let span = (obj.span.start, obj.span.end);
            let dst = cx.alloc_scratch();
            cx.emit(Op::NewObject, vec![Operand::Register(dst)], span);
            for prop in &obj.properties {
                match prop {
                    oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) => {
                        let key_span = (p.span.start, p.span.end);
                        // §13.2.5 Object Initializer — computed-key
                        // properties (`{ [expr]: value }`) lower to
                        // `Op::StoreElement` with the key value
                        // computed at runtime. Static-key paths
                        // keep the existing `Op::StoreProperty`
                        // fast path.
                        // <https://tc39.es/ecma262/#sec-object-initializer>
                        let static_key_str = if p.computed {
                            None
                        } else {
                            Some(match &p.key {
                                oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                                    id.name.as_str().to_string()
                                }
                                oxc_ast::ast::PropertyKey::StringLiteral(lit) => {
                                    lit.value.to_string()
                                }
                                oxc_ast::ast::PropertyKey::NumericLiteral(lit) => {
                                    lit.value.to_string()
                                }
                                _ => {
                                    return Err(CompileError::Unsupported {
                                        node: "ObjectExpression: non-string property key"
                                            .to_string(),
                                        span: key_span,
                                    });
                                }
                            })
                        };
                        if matches!(
                            p.kind,
                            oxc_ast::ast::PropertyKind::Get | oxc_ast::ast::PropertyKind::Set
                        ) {
                            let key_reg = match &static_key_str {
                                Some(key) => {
                                    let r = cx.alloc_scratch();
                                    let const_idx = cx.intern_string_constant(key);
                                    cx.emit(
                                        Op::LoadString,
                                        vec![Operand::Register(r), Operand::ConstIndex(const_idx)],
                                        key_span,
                                    );
                                    r
                                }
                                None => {
                                    let expr = p.key.as_expression().ok_or_else(|| {
                                        CompileError::Unsupported {
                                            node: "ObjectExpression: computed accessor key (non-expression)"
                                                .to_string(),
                                            span: key_span,
                                        }
                                    })?;
                                    compile_expr(cx, expr, key_span)?
                                }
                            };
                            let function_reg = compile_expr(cx, &p.value, key_span)?;
                            let desc_reg = cx.alloc_scratch();
                            cx.emit(Op::NewObject, vec![Operand::Register(desc_reg)], key_span);
                            let accessor_key = match p.kind {
                                oxc_ast::ast::PropertyKind::Get => "get",
                                oxc_ast::ast::PropertyKind::Set => "set",
                                oxc_ast::ast::PropertyKind::Init => unreachable!(),
                            };
                            let accessor_const = cx.intern_string_constant(accessor_key);
                            let store_scratch = cx.alloc_scratch();
                            cx.emit(
                                Op::StoreProperty,
                                vec![
                                    Operand::Register(desc_reg),
                                    Operand::ConstIndex(accessor_const),
                                    Operand::Register(function_reg),
                                    Operand::Register(store_scratch),
                                ],
                                key_span,
                            );
                            let true_reg = cx.alloc_scratch();
                            cx.emit(Op::LoadTrue, vec![Operand::Register(true_reg)], key_span);
                            for attr in ["enumerable", "configurable"] {
                                let attr_const = cx.intern_string_constant(attr);
                                let attr_scratch = cx.alloc_scratch();
                                cx.emit(
                                    Op::StoreProperty,
                                    vec![
                                        Operand::Register(desc_reg),
                                        Operand::ConstIndex(attr_const),
                                        Operand::Register(true_reg),
                                        Operand::Register(attr_scratch),
                                    ],
                                    key_span,
                                );
                            }
                            let define_dst = cx.alloc_scratch();
                            cx.emit(
                                Op::ObjectCall,
                                vec![
                                    Operand::Register(define_dst),
                                    Operand::ConstIndex(
                                        otter_bytecode::method_id::ObjectMethod::DefineProperty
                                            .as_u32(),
                                    ),
                                    Operand::ConstIndex(3),
                                    Operand::Register(dst),
                                    Operand::Register(key_reg),
                                    Operand::Register(desc_reg),
                                ],
                                key_span,
                            );
                            continue;
                        }
                        if p.computed {
                            let key_reg = match &p.key {
                                oxc_ast::ast::PropertyKey::StaticIdentifier(_)
                                | oxc_ast::ast::PropertyKey::StringLiteral(_) => {
                                    // Even when the syntax is
                                    // computed, oxc still preserves
                                    // the literal — but we lower
                                    // through the dynamic path so
                                    // string / symbol identity
                                    // observably matches spec.
                                    let expr = p.key.as_expression().ok_or_else(|| {
                                        CompileError::Unsupported {
                                            node: "ObjectExpression: computed key (non-expression)"
                                                .to_string(),
                                            span: key_span,
                                        }
                                    })?;
                                    compile_expr(cx, expr, key_span)?
                                }
                                _ => {
                                    let expr = p.key.as_expression().ok_or_else(|| {
                                        CompileError::Unsupported {
                                            node: "ObjectExpression: computed key (non-expression)"
                                                .to_string(),
                                            span: key_span,
                                        }
                                    })?;
                                    compile_expr(cx, expr, key_span)?
                                }
                            };
                            let value_reg = compile_expr(cx, &p.value, key_span)?;
                            cx.emit_store_element(dst, key_reg, value_reg, key_span);
                            continue;
                        }
                        let key_str = static_key_str.expect("non-computed key resolved above");
                        let value_reg = compile_expr(cx, &p.value, key_span)?;
                        let const_idx = cx.intern_string_constant(&key_str);
                        let store_scratch = cx.alloc_scratch();
                        cx.emit(
                            Op::StoreProperty,
                            vec![
                                Operand::Register(dst),
                                Operand::ConstIndex(const_idx),
                                Operand::Register(value_reg),
                                Operand::Register(store_scratch),
                            ],
                            key_span,
                        );
                    }
                    // §13.2.5.5 PropertyDefinitionEvaluation —
                    // `{ ...source }` copies enumerable own
                    // properties from `source` onto the object
                    // under construction. Foundation lowers this as
                    // a `LoadElement`-loop over `Object.keys(source)`.
                    // The runtime helper in `vm` walks the source
                    // object once (Op::ObjectCall("keys", source)
                    // → array of keys → for each key, copy).
                    oxc_ast::ast::ObjectPropertyKind::SpreadProperty(s) => {
                        let s_span = (s.span.start, s.span.end);
                        let src = compile_expr(cx, &s.argument, s_span)?;
                        // Lower the spread to `Object.assign(dst, src)` via
                        // the typed [`ObjectMethod::Assign`].
                        let scratch = cx.alloc_scratch();
                        cx.emit(
                            Op::ObjectCall,
                            vec![
                                Operand::Register(scratch),
                                Operand::ConstIndex(
                                    otter_bytecode::method_id::ObjectMethod::Assign.as_u32(),
                                ),
                                Operand::ConstIndex(2),
                                Operand::Register(dst),
                                Operand::Register(src),
                            ],
                            s_span,
                        );
                    }
                }
            }
            Ok(dst)
        }

        Expression::FunctionExpression(f) => {
            let span = (f.span.start, f.span.end);
            let name =
                f.id.as_ref()
                    .map(|id| id.name.as_str().to_string())
                    .unwrap_or_else(|| "<anonymous>".to_string());
            let (function_id, captures) = compile_function_full(
                cx,
                &name,
                &f.params,
                &f.body,
                span,
                f.r#async,
                f.generator,
                false,
            )?;
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_function_id(function_id);
            emit_make_callable(cx, dst, const_idx, &captures, false, span);
            Ok(dst)
        }

        Expression::ArrowFunctionExpression(a) => {
            let span = (a.span.start, a.span.end);
            let (function_id, captures) = compile_arrow_function(cx, a, span)?;
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_function_id(function_id);
            emit_make_callable(cx, dst, const_idx, &captures, true, span);
            Ok(dst)
        }

        Expression::ClassExpression(class) => {
            let name = class.id.as_ref().map(|id| id.name.as_str().to_string());
            compile_class(cx, class, name.as_deref())
        }

        Expression::MetaProperty(meta) => {
            let span = (meta.span.start, meta.span.end);
            if meta.meta.name.as_str() == "new" && meta.property.name.as_str() == "target" {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadNewTarget, vec![Operand::Register(dst)], span);
                return Ok(dst);
            }
            // The only legal MetaProperty inside a module is
            // `import.meta`. The runtime materialises it as a
            // JsObject the linker passes in as param 1; we hoist
            // it into `import_meta_uv` at function entry so
            // closures capture it.
            //
            // Spec: <https://tc39.es/ecma262/#prod-ImportMeta>
            //       <https://tc39.es/ecma262/#sec-meta-properties-runtime-semantics-evaluation>
            if meta.meta.name.as_str() != "import" || meta.property.name.as_str() != "meta" {
                return Err(CompileError::Unsupported {
                    node: format!(
                        "MetaProperty other than `import.meta` ({}.{})",
                        meta.meta.name, meta.property.name
                    ),
                    span,
                });
            }
            let import_meta_uv = cx.module_state.as_ref().map(|s| s.import_meta_uv).ok_or(
                CompileError::Unsupported {
                    node: "`import.meta` outside an ES-module fragment".to_string(),
                    span,
                },
            )?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadUpvalue,
                vec![
                    Operand::Register(dst),
                    Operand::Imm32(import_meta_uv as i32),
                ],
                span,
            );
            Ok(dst)
        }

        Expression::ImportExpression(imp) => {
            // §16.2.1.7 ImportCall: literal-string specifiers are
            // pre-resolved by the linker (synchronous namespace lookup
            // wrapped in a fulfilled promise). Non-literal specifiers
            // route through `Op::ImportNamespaceDynamic` which always
            // returns a [`crate::Value::Promise`] directly — fulfilled
            // for a specifier that resolves against the pre-linked
            // module graph; rejected with a TypeError when the runtime
            // cannot satisfy the specifier (no on-demand loader for
            // brand-new modules in this slice).
            //
            // Spec: <https://tc39.es/ecma262/#sec-import-call-runtime-semantics-evaluation>
            let span = (imp.span.start, imp.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "dynamic `import()` outside an ES-module fragment".to_string(),
                    span,
                });
            }
            match unwrap_ts_expr(&imp.source) {
                Expression::StringLiteral(lit) => {
                    // Literal: linker resolves it during fragment merge,
                    // opcode reads namespace + wraps in a fulfilled
                    // promise.
                    let specifier = lit.value.as_str().to_string();
                    let spec_const = cx.intern_string_constant(&specifier);
                    let ns_dst = cx.alloc_scratch();
                    cx.emit(
                        Op::ImportNamespace,
                        vec![Operand::Register(ns_dst), Operand::ConstIndex(spec_const)],
                        span,
                    );
                    let promise_dst = cx.alloc_scratch();
                    cx.emit(
                        Op::PromiseFulfilledOf,
                        vec![Operand::Register(promise_dst), Operand::Register(ns_dst)],
                        span,
                    );
                    Ok(promise_dst)
                }
                other => {
                    // Non-literal: opcode returns a Promise<namespace>
                    // (or Promise<TypeError>) directly, so no
                    // PromiseFulfilledOf wrap is needed.
                    let spec_reg = compile_expr(cx, other, span)?;
                    let promise_dst = cx.alloc_scratch();
                    cx.emit(
                        Op::ImportNamespaceDynamic,
                        vec![Operand::Register(promise_dst), Operand::Register(spec_reg)],
                        span,
                    );
                    Ok(promise_dst)
                }
            }
        }

        Expression::AwaitExpression(a) => {
            let span = (a.span.start, a.span.end);
            let src = compile_expr(cx, &a.argument, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::Await,
                vec![Operand::Register(dst), Operand::Register(src)],
                span,
            );
            Ok(dst)
        }

        // §15.5 — `yield expr` inside a generator body. Lowered to
        // [`Op::Yield`]; the result register receives whatever value
        // the next `.next(arg)` call passes back in. `yield*` is
        // not yet implemented and surfaces as `Unsupported`.
        // <https://tc39.es/ecma262/#sec-yield>
        Expression::YieldExpression(y) => {
            let span = (y.span.start, y.span.end);
            // §15.5.5 `yield*` — delegate to an inner iterable. The
            // foundation lowers it as the canonical for-of-style
            // pump:
            //
            //   const iter = GetIterator(arg);
            //   while (true) {
            //     const { value, done } = iter.next();
            //     if (done) { break; }       // value of yield* is `undefined`
            //     yield value;
            //   }
            //
            // Spec demands threading the resume value into iter.next
            // and forwarding `.return` / `.throw` through; both are
            // filed for a follow-up.
            // <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation>
            if y.delegate {
                let arg = match &y.argument {
                    Some(a) => a,
                    None => {
                        return Err(CompileError::Unsupported {
                            node: "yield*: missing argument".to_string(),
                            span,
                        });
                    }
                };
                let arg_reg = compile_expr(cx, arg, span)?;
                let iter_reg = cx.alloc_scratch();
                cx.emit(
                    Op::GetIterator,
                    vec![Operand::Register(iter_reg), Operand::Register(arg_reg)],
                    span,
                );
                let value_reg = cx.alloc_scratch();
                let done_reg = cx.alloc_scratch();
                let loop_top = cx.next_pc;
                cx.emit(
                    Op::IteratorNext,
                    vec![
                        Operand::Register(value_reg),
                        Operand::Register(done_reg),
                        Operand::Register(iter_reg),
                    ],
                    span,
                );
                let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
                let yield_dst = cx.alloc_scratch();
                cx.emit(
                    Op::Yield,
                    vec![Operand::Register(yield_dst), Operand::Register(value_reg)],
                    span,
                );
                let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
                cx.patch_branch(back_jmp, loop_top);
                cx.patch_branch_to_here(exit_jmp);
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
                return Ok(dst);
            }
            let src = match &y.argument {
                Some(arg) => compile_expr(cx, arg, span)?,
                None => {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
                    r
                }
            };
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::Yield,
                vec![Operand::Register(dst), Operand::Register(src)],
                span,
            );
            Ok(dst)
        }

        // §13.4 Postfix / Prefix update — `i++` / `++i` / `i--` /
        // `--i`. Foundation handles Identifier targets only; member
        // and computed-member operands fall through to Unsupported
        // (a subsequent slice covers them when test262 surfaces a
        // matching gap).
        // <https://tc39.es/ecma262/#sec-update-expressions>
        Expression::UpdateExpression(u) => {
            let span = (u.span.start, u.span.end);
            // §13.4 UpdateExpression — applies to identifiers,
            // static / computed member access, and (private-field
            // access — deferred). Refactor: compute a load + store
            // closure pair so the increment / decrement loop is
            // shared across target shapes.
            // <https://tc39.es/ecma262/#sec-update-expressions>
            let old = cx.alloc_scratch();
            // Storage closure result: a function the store path
            // reads to write `next` back to the same target.
            enum UpdateTarget<'b, 'c> {
                Identifier {
                    name: String,
                    storage: Option<BindingStorage>,
                },
                StaticMember {
                    obj_reg: u16,
                    name: &'b str,
                },
                ComputedMember {
                    obj_reg: u16,
                    key_reg: u16,
                    _phantom: std::marker::PhantomData<&'c ()>,
                },
            }
            let target = match &u.argument {
                SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                    let name = id.name.as_str().to_string();
                    let storage = match cx.lookup_binding(&name) {
                        Some(info) if info.is_const => {
                            return Err(CompileError::Unsupported {
                                node: format!("update on const `{name}`"),
                                span,
                            });
                        }
                        Some(info) => Some(info.storage),
                        None => cx
                            .resolve_capture(&name)
                            .map(|idx| BindingStorage::Upvalue { idx }),
                    };
                    match storage {
                        Some(s) => cx.emit_load_storage(old, s, span),
                        None => {
                            let global = cx.alloc_scratch();
                            cx.emit(Op::LoadGlobalThis, vec![Operand::Register(global)], span);
                            cx.emit_load_property(old, global, &name, span);
                        }
                    }
                    UpdateTarget::Identifier { name, storage }
                }
                SimpleAssignmentTarget::StaticMemberExpression(member) => {
                    let obj_reg = compile_expr(cx, &member.object, span)?;
                    let name = member.property.name.as_str();
                    cx.emit_load_property(old, obj_reg, name, span);
                    UpdateTarget::StaticMember { obj_reg, name }
                }
                SimpleAssignmentTarget::ComputedMemberExpression(member) => {
                    let obj_reg = compile_expr(cx, &member.object, span)?;
                    let key_reg = compile_expr(cx, &member.expression, span)?;
                    cx.emit(
                        Op::LoadElement,
                        vec![
                            Operand::Register(old),
                            Operand::Register(obj_reg),
                            Operand::Register(key_reg),
                        ],
                        span,
                    );
                    UpdateTarget::ComputedMember {
                        obj_reg,
                        key_reg,
                        _phantom: std::marker::PhantomData,
                    }
                }
                _ => {
                    return Err(CompileError::Unsupported {
                        node: "UpdateExpression on non-identifier operand".to_string(),
                        span,
                    });
                }
            };

            // §13.4 step 3 — coerce to number (or BigInt via
            // ToNumeric). The shared run-numeric path now handles
            // primitives so `cur` can flow through Add/Sub directly.
            let cur = cx.alloc_scratch();
            cx.emit(
                Op::ToNumber,
                vec![Operand::Register(cur), Operand::Register(old)],
                span,
            );
            let one = cx.alloc_scratch();
            cx.emit(
                Op::LoadInt32,
                vec![Operand::Register(one), Operand::Imm32(1)],
                span,
            );
            let next = cx.alloc_scratch();
            let op = match u.operator {
                UpdateOperator::Increment => Op::Add,
                UpdateOperator::Decrement => Op::Sub,
            };
            cx.emit(
                op,
                vec![
                    Operand::Register(next),
                    Operand::Register(cur),
                    Operand::Register(one),
                ],
                span,
            );
            match target {
                UpdateTarget::Identifier { name, storage } => match storage {
                    Some(s) => cx.emit_store_storage(next, s, span),
                    None => {
                        let global = cx.alloc_scratch();
                        cx.emit(Op::LoadGlobalThis, vec![Operand::Register(global)], span);
                        let name_idx = cx.intern_string_constant(&name);
                        let scratch = cx.alloc_scratch();
                        cx.emit(
                            Op::StoreProperty,
                            vec![
                                Operand::Register(global),
                                Operand::ConstIndex(name_idx),
                                Operand::Register(next),
                                Operand::Register(scratch),
                            ],
                            span,
                        );
                    }
                },
                UpdateTarget::StaticMember { obj_reg, name } => {
                    let name_idx = cx.intern_string_constant(name);
                    let scratch = cx.alloc_scratch();
                    cx.emit(
                        Op::StoreProperty,
                        vec![
                            Operand::Register(obj_reg),
                            Operand::ConstIndex(name_idx),
                            Operand::Register(next),
                            Operand::Register(scratch),
                        ],
                        span,
                    );
                }
                UpdateTarget::ComputedMember {
                    obj_reg, key_reg, ..
                } => {
                    cx.emit_store_element(obj_reg, key_reg, next, span);
                }
            }
            // §13.4.2.1 / 13.4.3.1 — postfix returns the pre-
            // update value (post-ToNumber); prefix returns the
            // new value.
            Ok(if u.prefix { next } else { cur })
        }

        other => Err(CompileError::Unsupported {
            node: format!("Expression ({})", expr_kind_name(other)),
            span: expr_span(other),
        }),
    }
}

/// Lower `for (let x of expr) { body }` to the foundation
/// iterator-protocol shape:
///
/// ```text
///   tmp_iter = GetIterator(expr)
///   loop_top:
///   IteratorNext value, done, tmp_iter
///   JumpIfTrue done -> loop_exit
///   <bind value into the loop variable>
///   <body>
///   Jump -> loop_top
///   loop_exit:
/// ```
///
/// The loop variable lives in a fresh scope per iteration so a
/// `let`-declared binding does not leak between iterations or to
/// the outside. `break` lands at `loop_exit`; `continue` jumps to
/// the top so a fresh value is fetched. Real iterator-close
/// semantics (running an `[@@return]` hook on early termination)
/// land alongside generators in a later slice.
fn compile_for_of_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::ForOfStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    let is_for_await = s.r#await;

    let iterable_reg = compile_expr(cx, &s.right, span)?;
    let iter_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetIterator,
        vec![Operand::Register(iter_reg), Operand::Register(iterable_reg)],
        span,
    );

    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();

    cx.push_loop_frame(LoopFrame::iteration());
    let loop_top = cx.next_pc;
    cx.emit(
        Op::IteratorNext,
        vec![
            Operand::Register(value_reg),
            Operand::Register(done_reg),
            Operand::Register(iter_reg),
        ],
        span,
    );
    let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);

    // §14.7.5.6 step `for await … of` — per the iteration semantics
    // for async iterables, the value produced by each step must be
    // awaited before binding it to the loop variable. The
    // foundation lowers `Op::IteratorNext` against a `Value::Generator`
    // synchronously (the helper unwraps the gen's `{value, done}`
    // record before re-emerging here), so by awaiting the value we
    // both unwrap any user-yielded Promises and stay
    // spec-compatible with sync-iterable inputs (await of a
    // non-thenable resolves to the value itself).
    // <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements>
    let bind_source = if is_for_await {
        let awaited = cx.alloc_scratch();
        cx.emit(
            Op::Await,
            vec![Operand::Register(awaited), Operand::Register(value_reg)],
            span,
        );
        awaited
    } else {
        value_reg
    };

    // §14.7.5.6 ForIn/OfBodyEvaluation: `let`/`const` re-bind per
    // iteration in a fresh lexical scope; `var` writes back into
    // the function-scope binding pre-hoisted at function entry.
    // AssignmentTarget heads reassign without a fresh scope per
    // step (no per-iteration binding to materialize).
    cx.enter_scope();
    bind_for_in_of_head(cx, &s.left, bind_source, span)?;
    compile_statement(cx, &s.body)?;
    cx.exit_scope();

    let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch(back_jmp, loop_top);
    cx.patch_branch_to_here(exit_jmp);

    let frame = cx.loops.pop().expect("for-of loop frame");
    for pc in frame.continue_patches {
        cx.patch_branch(pc, loop_top);
    }
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    Ok(None)
}

/// Bind the per-iteration value of a `for-in` / `for-of` head to the
/// declared / pre-existing target. Handles the four head shapes oxc
/// produces:
///
/// 1. `for (let x of …)` / `const` / `var` with a plain identifier,
/// 2. `for (let [a, b] of …)` etc. with a destructuring pattern,
/// 3. `for (x of …)` — assignment to an existing identifier,
/// 4. `for (obj.prop of …)` / `for ([a, b] of …)` etc. — assignment
///    to a member expression or destructuring assignment target.
///
/// Spec: <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements>
/// (ForIn/OfBodyEvaluation).
fn bind_for_in_of_head(
    cx: &mut Compiler,
    head: &oxc_ast::ast::ForStatementLeft<'_>,
    src_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
    use oxc_ast::ast::{BindingPattern, ForStatementLeft, VariableDeclarationKind};
    match head {
        ForStatementLeft::VariableDeclaration(decl) => {
            if decl.declarations.len() != 1 {
                return Err(CompileError::Unsupported {
                    node: "ForOfStatement: multi-declarator head".to_string(),
                    span,
                });
            }
            let declarator = &decl.declarations[0];
            let is_const = matches!(decl.kind, VariableDeclarationKind::Const);
            let is_var = matches!(decl.kind, VariableDeclarationKind::Var);
            match &declarator.id {
                BindingPattern::BindingIdentifier(id) => {
                    let name = id.name.as_str().to_string();
                    let storage = if is_var {
                        cx.lookup_binding(&name)
                            .ok_or(CompileError::Unsupported {
                                node: format!("for-of var `{name}` not pre-hoisted"),
                                span,
                            })?
                            .storage
                    } else {
                        cx.declare_binding(&name, is_const, span)?
                    };
                    cx.emit_store_storage(src_reg, storage, span);
                    cx.mark_initialized(&name);
                    Ok(())
                }
                _ => {
                    if is_var {
                        // §14.7.5.6 step 6.b — for `var` heads, the
                        // pattern leaves were already var-hoisted
                        // at function entry; per iteration we just
                        // assign into those existing bindings.
                        destructure_assign(cx, src_reg, &declarator.id, span)
                    } else {
                        // For let/const heads, declare each leaf
                        // per iteration in the fresh scope.
                        destructure_into(cx, src_reg, &declarator.id, span)
                    }
                }
            }
        }
        // `for (target of …)` — AssignmentTarget. Reuse the
        // shared `assign_to_target` helper which handles
        // identifier / member / array-pattern / object-pattern.
        // We pattern-match each variant explicitly to translate
        // ForStatementLeft → AssignmentTarget without unsafe.
        ForStatementLeft::AssignmentTargetIdentifier(id) => {
            store_identifier(cx, id.name.as_str(), src_reg, span)
        }
        ForStatementLeft::ArrayAssignmentTarget(arr) => {
            assign_array_pattern(cx, arr, src_reg, span)
        }
        ForStatementLeft::ObjectAssignmentTarget(obj) => {
            assign_object_pattern(cx, obj, src_reg, span)
        }
        ForStatementLeft::StaticMemberExpression(member) => {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let scratch = cx.alloc_scratch();
            cx.emit(
                Op::StoreProperty,
                vec![
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                    Operand::Register(src_reg),
                    Operand::Register(scratch),
                ],
                span,
            );
            Ok(())
        }
        ForStatementLeft::ComputedMemberExpression(member) => {
            let obj_reg = compile_expr(cx, &member.object, span)?;
            let key_reg = compile_expr(cx, &member.expression, span)?;
            cx.emit_store_element(obj_reg, key_reg, src_reg, span);
            Ok(())
        }
        // TS-only wrapper variants — unwrap the inner target.
        ForStatementLeft::TSAsExpression(_)
        | ForStatementLeft::TSSatisfiesExpression(_)
        | ForStatementLeft::TSNonNullExpression(_)
        | ForStatementLeft::TSTypeAssertion(_) => Err(CompileError::Unsupported {
            node: "ForOfStatement: TS-wrapped target head".to_string(),
            span,
        }),
        ForStatementLeft::PrivateFieldExpression(_) => Err(CompileError::Unsupported {
            node: "ForOfStatement: private-field target head".to_string(),
            span,
        }),
    }
}

/// Lower `switch (disc) { case ...; default: ...; }` per ECMA-262
/// §14.12 SwitchStatement.
///
/// # Algorithm
/// 1. Evaluate the discriminant once into a scratch register.
/// 2. Walk every `case label:` and emit a strict-equality compare
///    (`disc === label`) followed by `JUMP_IF_TRUE → case_body_pc`.
///    Patch the jump targets after we know each case body's pc.
/// 3. After all case probes, emit one unconditional `JUMP` to the
///    `default:` body when present, or to the switch end otherwise.
///    This implements the spec's two-pass `CaseSelector` evaluation:
///    cases run in source order, then `default` if no case matched.
/// 4. Compile every case body in source order, falling through into
///    the next on missing `break`. Each body's start pc is captured
///    so step 2's placeholders can be patched.
/// 5. Push a [`LoopFrame::switch_frame`] so `break` targets the
///    end of the switch and `continue` skips the frame entirely
///    (per §13.10.1).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-switch-statement>
/// - <https://tc39.es/ecma262/#sec-runtime-semantics-caseclauseisselected>
fn compile_switch_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::SwitchStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    let disc_reg = compile_expr(cx, &s.discriminant, span)?;

    // Fresh lexical scope so per-case `let` bindings don't leak.
    cx.enter_scope();
    cx.push_loop_frame(LoopFrame::switch_body());

    // Pass 1: emit selector comparisons for every non-default case.
    // The placeholders carry the target PC inside the JUMP_IF_TRUE
    // operand; we patch them once each body's pc is known.
    let mut case_jump_pcs: Vec<(usize, u32)> = Vec::with_capacity(s.cases.len());
    let mut default_idx: Option<usize> = None;
    for (idx, case) in s.cases.iter().enumerate() {
        let case_span = (case.span.start, case.span.end);
        match &case.test {
            Some(test) => {
                let test_reg = compile_expr(cx, test, case_span)?;
                let cmp_reg = cx.alloc_scratch();
                // §13.11.1 strict equality.
                cx.emit(
                    Op::Equal,
                    vec![
                        Operand::Register(cmp_reg),
                        Operand::Register(disc_reg),
                        Operand::Register(test_reg),
                    ],
                    case_span,
                );
                let placeholder =
                    cx.emit_branch_placeholder(Op::JumpIfTrue, Some(cmp_reg), case_span);
                case_jump_pcs.push((idx, placeholder));
            }
            None => {
                if default_idx.is_some() {
                    return Err(CompileError::Unsupported {
                        node: "SwitchStatement: multiple default clauses".to_string(),
                        span: case_span,
                    });
                }
                default_idx = Some(idx);
            }
        }
    }
    // Fall-through after all case probes — jump to default body if
    // present, else to the end of the switch.
    let default_jump = cx.emit_branch_placeholder(Op::Jump, None, span);

    // Pass 2: compile each case body in source order.
    let mut case_body_pcs: Vec<u32> = Vec::with_capacity(s.cases.len());
    for case in s.cases.iter() {
        let body_pc = cx.next_pc;
        case_body_pcs.push(body_pc);
        for inner in case.consequent.iter() {
            compile_statement(cx, inner)?;
        }
    }
    let switch_end_pc = cx.next_pc;

    // Patch case selector jumps to their body pc.
    for (idx, placeholder) in case_jump_pcs {
        cx.patch_branch(placeholder, case_body_pcs[idx]);
    }
    // Patch the post-probe fall-through jump.
    match default_idx {
        Some(idx) => cx.patch_branch(default_jump, case_body_pcs[idx]),
        None => cx.patch_branch(default_jump, switch_end_pc),
    }

    // Patch every `break` inside the switch to land at the end.
    let frame = cx.loops.pop().expect("switch frame disappeared");
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    debug_assert!(
        frame.continue_patches.is_empty(),
        "switch frames must not collect continue patches"
    );
    cx.exit_scope();
    Ok(None)
}

/// Lower `for (k in obj) { … }` per ECMA-262 §14.7.5.6
/// `ForIn/OfHeadEvaluation` + §14.7.5.10 EnumerateObjectProperties.
///
/// # Algorithm
/// 1. Evaluate the right-hand side. If it is `null` / `undefined`
///    the loop is silently skipped (§14.7.5.6 step 7.b).
/// 2. Snapshot the receiver's enumerable own + inherited string
///    keys at loop entry. Foundation keeps the snapshot static —
///    spec §14.7.5.10's "iterate keys created during enumeration"
///    is filed against a follow-up.
/// 3. Walk the snapshot via an integer counter; on each iteration
///    re-bind the loop variable in a fresh per-iteration scope so
///    `let k in o` matches §14.7.5.6 step 7.f.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements>
/// - <https://tc39.es/ecma262/#sec-enumerate-object-properties>
fn compile_for_in_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::ForInStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);

    // Foundation lowering: convert `for (k in o)` into
    // `for (k of Object.keys(o))` using the existing iterator
    // machinery. The key set is captured at loop entry per
    // step (2) above. `Object.keys` returns own enumerable
    // string-keyed properties — close enough to the spec's
    // EnumerateObjectProperties for foundation use cases; full
    // proto-chain enumeration is filed as a follow-up.
    //
    // We emit:
    //   r_obj = <right>;
    //   r_keys = Object.keys(r_obj);   // Op::ObjectCall
    //   r_iter = GetIterator(r_keys);
    //   loop_top:
    //     IteratorNext r_value, r_done, r_iter
    //     JumpIfTrue r_done -> exit
    //     <bind let k = r_value>
    //     <body>
    //     Jump loop_top
    //   exit:
    let obj_reg = compile_expr(cx, &s.right, span)?;
    let keys_reg = cx.alloc_scratch();
    cx.emit(
        Op::ObjectCall,
        vec![
            Operand::Register(keys_reg),
            Operand::ConstIndex(otter_bytecode::method_id::ObjectMethod::Keys.as_u32()),
            Operand::ConstIndex(1),
            Operand::Register(obj_reg),
        ],
        span,
    );

    let iter_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetIterator,
        vec![Operand::Register(iter_reg), Operand::Register(keys_reg)],
        span,
    );

    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();

    cx.push_loop_frame(LoopFrame::iteration());
    let loop_top = cx.next_pc;
    cx.emit(
        Op::IteratorNext,
        vec![
            Operand::Register(value_reg),
            Operand::Register(done_reg),
            Operand::Register(iter_reg),
        ],
        span,
    );
    let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);

    // §14.7.5.6 — `let`/`const` rebinds per iteration; `var`
    // re-uses the function-scope binding. Assignment-target heads
    // reassign in place.
    cx.enter_scope();
    bind_for_in_of_head(cx, &s.left, value_reg, span)?;
    compile_statement(cx, &s.body)?;
    cx.exit_scope();

    let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch(back_jmp, loop_top);
    cx.patch_branch_to_here(exit_jmp);

    let frame = cx.loops.pop().expect("for-in loop frame");
    for pc in frame.continue_patches {
        cx.patch_branch(pc, loop_top);
    }
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    Ok(None)
}

/// Lower `label: stmt` per ECMA-262 §14.13.
///
/// Foundation supports labels on iteration statements and `switch`
/// only — every other shape (block-statement label, function
/// declaration, etc.) is theoretically valid spec-wise but the
/// foundation keeps the surface tight. Adding the missing shapes is
/// purely a matter of stamping the label onto a fresh
/// [`LoopFrame::switch_frame`]-style entry and patching break
/// targets at scope close.
///
/// The label is attached to the enclosed statement by injecting it
/// into the loop / switch frame the inner statement pushes. We
/// detect the label site, stash the name in the next-pushed
/// frame, and recurse into the body — the inner compile_*
/// helpers consult `cx.loops.last()` already, so the label is
/// visible to nested `break label;` / `continue label;`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-labelled-statements>
fn compile_labeled_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::LabeledStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    let span = (s.span.start, s.span.end);
    let label = s.label.name.as_str().to_string();
    // Reject duplicate labels in the enclosing chain — §14.13.1
    // early error.
    if cx
        .loops
        .iter()
        .any(|f| f.label.as_deref() == Some(label.as_str()))
    {
        return Err(CompileError::Unsupported {
            node: format!("LabeledStatement: duplicate label `{label}` in enclosing chain"),
            span,
        });
    }
    // §14.13.4 — labels may wrap any Statement. Three cases:
    //
    // 1. The body is a loop / switch: stash the label as
    //    `pending_label`; the loop's frame consumes it on push.
    // 2. The body is a Block / If / Try / etc.: push a synthetic
    //    "labeled-block" frame so `break label;` from the body
    //    can branch past the block. `continue label;` is a
    //    SyntaxError for non-iteration labels per §13.8.1
    //    (foundation surfaces it at runtime via the loop walk).
    // 3. The body is a plain statement (`a: 1`): nothing to do
    //    after compiling the body.
    use oxc_ast::ast::Statement;
    let body_takes_loop_label = matches!(
        &s.body,
        Statement::ForStatement(_)
            | Statement::ForInStatement(_)
            | Statement::ForOfStatement(_)
            | Statement::WhileStatement(_)
            | Statement::DoWhileStatement(_)
            | Statement::SwitchStatement(_)
    );
    if body_takes_loop_label {
        let prev_pending = cx.pending_label.replace(label);
        let result = compile_statement(cx, &s.body);
        cx.pending_label = prev_pending;
        return result;
    }
    // Synthetic labeled block — push a non-loop frame so
    // `break label;` from inside the body has somewhere to land.
    let mut frame = LoopFrame::switch_body();
    frame.label = Some(label);
    cx.push_loop_frame(frame);
    let result = compile_statement(cx, &s.body);
    let frame = cx.loops.pop().expect("labeled-block frame");
    for pc in frame.break_patches {
        cx.patch_branch_to_here(pc);
    }
    let _ = span;
    result
}

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
fn compile_try_statement(
    cx: &mut Compiler,
    s: &oxc_ast::ast::TryStatement<'_>,
) -> Result<Option<u16>, CompileError> {
    use otter_bytecode::NO_HANDLER_OFFSET;

    let span = (s.span.start, s.span.end);
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
        let inner = cx.emit_enter_try(0, NO_HANDLER_OFFSET, exc_reg, span);

        cx.enter_scope();
        for inner_stmt in &s.block.body {
            compile_statement(cx, inner_stmt)?;
        }
        cx.exit_scope();
        cx.emit(Op::LeaveTry, vec![], span);
        let success_jump = cx.emit_branch_placeholder(Op::Jump, None, span);

        cx.patch_enter_try_offset(inner, /* catch */ true);
        compile_catch_clause(cx, s.handler.as_ref().unwrap(), exc_reg, body_span)?;

        cx.patch_branch_to_here(success_jump);

        cx.patch_enter_try_offset(outer, /* finally */ false);
        cx.emit(Op::LeaveTry, vec![], span);
        compile_finalizer(cx, s.finalizer.as_ref().unwrap())?;
        cx.emit(Op::EndFinally, vec![], span);
        return Ok(None);
    }

    if has_catch {
        let handler_pc = cx.emit_enter_try(0, NO_HANDLER_OFFSET, exc_reg, span);
        cx.enter_scope();
        for inner_stmt in &s.block.body {
            compile_statement(cx, inner_stmt)?;
        }
        cx.exit_scope();
        cx.emit(Op::LeaveTry, vec![], span);
        let skip_catch = cx.emit_branch_placeholder(Op::Jump, None, span);

        cx.patch_enter_try_offset(handler_pc, true);
        compile_catch_clause(cx, s.handler.as_ref().unwrap(), exc_reg, body_span)?;

        cx.patch_branch_to_here(skip_catch);
        return Ok(None);
    }

    // try / finally only.
    let handler_pc = cx.emit_enter_try(NO_HANDLER_OFFSET, 0, exc_reg, span);
    cx.enter_scope();
    for inner_stmt in &s.block.body {
        compile_statement(cx, inner_stmt)?;
    }
    cx.exit_scope();
    cx.emit(Op::LeaveTry, vec![], span);
    cx.patch_enter_try_offset(handler_pc, false);
    compile_finalizer(cx, s.finalizer.as_ref().unwrap())?;
    cx.emit(Op::EndFinally, vec![], span);
    Ok(None)
}

fn compile_catch_clause(
    cx: &mut Compiler,
    handler: &oxc_ast::ast::CatchClause<'_>,
    exc_reg: u16,
    span: (u32, u32),
) -> Result<(), CompileError> {
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

fn compile_finalizer(
    cx: &mut Compiler,
    finalizer: &oxc_ast::ast::BlockStatement<'_>,
) -> Result<(), CompileError> {
    cx.enter_scope();
    for inner in &finalizer.body {
        compile_statement(cx, inner)?;
    }
    cx.exit_scope();
    Ok(())
}

/// Lower a call expression. Three forms are supported:
///
/// - `receiver.method(args...)` — emits [`Op::CallMethodValue`].
///   The runtime branches by receiver kind (string / array
///   intrinsics, plain object property dispatch, or
///   `Function.prototype.{call, apply, bind}` for callables).
/// - `callee.{call, apply, bind}(...)` with a syntactically obvious
///   call shape — lowered directly to [`Op::CallWithThis`] /
///   [`Op::BindFunction`] when the argument list can be flattened at
///   compile time. Dynamic `apply` argument lists stay on
///   [`Op::CallMethodValue`] so the VM performs the spec
///   `CreateListFromArrayLike` coercion.
/// - `callee(args...)` (free call) — emits [`Op::Call`]; the callee
///   receives `this = undefined`.
///
/// Computed-method access, `new`, and spread arguments are
/// deferred to later tasks.
fn compile_method_call(
    cx: &mut Compiler,
    call: &oxc_ast::ast::CallExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (call.span.start, call.span.end);
    let callee = unwrap_ts_expr(&call.callee);
    // `super(args...)` — direct super-constructor call. Only valid
    // inside a derived-class constructor; the upvalue lookup will
    // surface a clear diagnostic when used elsewhere.
    if let Expression::Super(_) = callee {
        return compile_super_call(cx, &call.arguments, span);
    }
    // `super.foo(args...)` — invoke a parent prototype method with
    // `this` bound to the current receiver.
    if let Expression::StaticMemberExpression(member) = callee
        && matches!(member.object, Expression::Super(_))
    {
        return compile_super_method_call(cx, member.property.name.as_str(), &call.arguments, span);
    }
    // `import.meta.resolve(specifier)` — sync URL join against the
    // active module's URL. HTML spec returns a string; foundation
    // matches that shape via `Op::ImportMetaResolve`.
    // <https://html.spec.whatwg.org/multipage/webappapis.html#hostmetagetimportmetaproperties>
    if let Expression::StaticMemberExpression(member) = callee
        && let Expression::MetaProperty(meta) = &member.object
        && meta.meta.name.as_str() == "import"
        && meta.property.name.as_str() == "meta"
        && member.property.name.as_str() == "resolve"
    {
        if call.arguments.len() != 1 {
            return Err(CompileError::Unsupported {
                node: format!("import.meta.resolve/{}", call.arguments.len()),
                span,
            });
        }
        let arg = call.arguments[0]
            .as_expression()
            .ok_or(CompileError::Unsupported {
                node: "import.meta.resolve: spread argument".to_string(),
                span,
            })?;
        let spec_reg = compile_expr(cx, arg, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::ImportMetaResolve,
            vec![Operand::Register(dst), Operand::Register(spec_reg)],
            span,
        );
        return Ok(dst);
    }
    // Bare `Error("msg")` / `TypeError("msg")` / etc. without
    // `new` is treated like the matching `new <Kind>("msg")` per
    // ES spec §20.5.1.1 — same lowering.
    if let Expression::Identifier(id) = callee
        && cx.lookup_binding(id.name.as_str()).is_none()
        && find_module_import_binding(cx, id.name.as_str()).is_none()
        && is_builtin_error_class_name(id.name.as_str())
        && builtin_error_construct_fast_path_applies(id.name.as_str(), &call.arguments)
    {
        return compile_builtin_error_construct(cx, id.name.as_str(), &call.arguments, span);
    }
    let has_spread = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    if has_spread {
        return compile_spread_call(cx, callee, &call.arguments, span);
    }
    if let Expression::StaticMemberExpression(member) = callee {
        // §25.1.4.3 `ArrayBuffer.isView(arg)` — lower through
        // `Op::ArrayBufferCall`.
        // <https://tc39.es/ecma262/#sec-arraybuffer.isview>
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "ArrayBuffer"
            && cx.lookup_binding("ArrayBuffer").is_none()
            && find_module_import_binding(cx, "ArrayBuffer").is_none()
        {
            let method = member.property.name.as_str();
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let name_idx = cx.intern_string_constant(method);
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(name_idx));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::ArrayBufferCall, operands, span);
            return Ok(dst);
        }
        // §28.2.2 `Proxy.<method>(target, handler)` — typed dispatch
        // via [`ProxyMethod`].
        // <https://tc39.es/ecma262/#sec-proxy.revocable>
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Proxy"
            && cx.lookup_binding("Proxy").is_none()
            && find_module_import_binding(cx, "Proxy").is_none()
        {
            let method_name = member.property.name.as_str();
            let Some(method_id) = otter_bytecode::method_id::ProxyMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Proxy.{method_name}"),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::ProxyCall, operands, span);
            return Ok(dst);
        }
        // §25.4 `Atomics.<method>(args)` — typed dispatch via
        // [`AtomicsMethod`].
        // <https://tc39.es/ecma262/#sec-atomics-object>
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Atomics"
            && cx.lookup_binding("Atomics").is_none()
            && find_module_import_binding(cx, "Atomics").is_none()
        {
            let method_name = member.property.name.as_str();
            let Some(method_id) = otter_bytecode::method_id::AtomicsMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Atomics.{method_name}"),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::AtomicsCall, operands, span);
            return Ok(dst);
        }
        // §28.1 `Reflect.<method>(args)` — typed dispatch via
        // [`ReflectMethod`].
        // <https://tc39.es/ecma262/#sec-reflect-object>
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Reflect"
            && cx.lookup_binding("Reflect").is_none()
            && find_module_import_binding(cx, "Reflect").is_none()
        {
            let method_name = member.property.name.as_str();
            let Some(method_id) = otter_bytecode::method_id::ReflectMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Reflect.{method_name}"),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::ReflectCall, operands, span);
            return Ok(dst);
        }
        // Iterator-helpers proposal — `Iterator.from(iter)` and
        // future statics. Typed dispatch via [`IteratorMethod`].
        // <https://tc39.es/proposal-iterator-helpers/#sec-iterator.from>
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Iterator"
            && cx.lookup_binding("Iterator").is_none()
            && find_module_import_binding(cx, "Iterator").is_none()
        {
            let method_name = member.property.name.as_str();
            let Some(method_id) = otter_bytecode::method_id::IteratorMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Iterator.{method_name}"),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::IteratorCall, operands, span);
            return Ok(dst);
        }
        // §23.2.2 TypedArray statics — `<T>.from(...)` / `<T>.of(...)`.
        // Encodes the kind discriminant plus the typed
        // [`TypedArrayMethod`].
        // <https://tc39.es/ecma262/#sec-properties-of-the-%25typedarray%25-intrinsic-object>
        if let Expression::Identifier(id) = &member.object
            && let Some(kind) =
                otter_bytecode::method_id::TypedArrayKindId::from_str(id.name.as_str())
            && cx.lookup_binding(id.name.as_str()).is_none()
            && find_module_import_binding(cx, id.name.as_str()).is_none()
        {
            let method_name = member.property.name.as_str();
            let Some(method_id) =
                otter_bytecode::method_id::TypedArrayMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("{}.{method_name}", kind.name()),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(kind.as_u32()));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::TypedArrayCall, operands, span);
            return Ok(dst);
        }
        // Foundation built-ins on the global `Object`: lower a few
        // canonical forms directly to dedicated opcodes so the
        // runtime does not need a host-callable bridge yet.
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Object"
        {
            let method = member.property.name.as_str();
            if is_compiler_lowered_object_static(method) {
                let arg_regs = compile_call_args(cx, &call.arguments, span)?;
                return compile_object_builtin(cx, method, &arg_regs, span);
            }
        }
        // §23.1.2 Array static surface. `Array.isArray` keeps a
        // dedicated [`Op::IsArray`] for the §7.2.2 fast path;
        // `Array.from` / `Array.of` lower to dedicated
        // [`Op::ArrayFrom`] / [`Op::ArrayOf`] opcodes.
        // <https://tc39.es/ecma262/#sec-properties-of-the-array-constructor>
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Array"
        {
            let method = member.property.name.as_str();
            if method == "isArray" {
                let arg_regs = compile_call_args(cx, &call.arguments, span)?;
                if arg_regs.len() != 1 {
                    return Err(CompileError::Unsupported {
                        node: format!("Array.isArray/{}", arg_regs.len()),
                        span,
                    });
                }
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::IsArray,
                    vec![Operand::Register(dst), Operand::Register(arg_regs[0])],
                    span,
                );
                return Ok(dst);
            }
            if matches!(method, "from" | "of") {
                let arg_regs = compile_call_args(cx, &call.arguments, span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.iter().copied().map(Operand::Register));
                let opcode = if method == "from" {
                    Op::ArrayFrom
                } else {
                    Op::ArrayOf
                };
                cx.emit(opcode, operands, span);
                return Ok(dst);
            }
        }
        // `Math.<name>(args)` — typed dispatch via [`MathMethod`].
        // Constant-style names (`PI`, `E`, …) load through the
        // separate [`Op::MathLoad`] path, so an unknown method here
        // surfaces as a compile error.
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Math"
        {
            let method_name = member.property.name.as_str();
            let Some(method_id) = otter_bytecode::method_id::MathMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Math.{method_name}"),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::MathCall, operands, span);
            return Ok(dst);
        }
        // `JSON.<name>(args)` — typed dispatch via [`JsonMethod`].
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "JSON"
        {
            let method_name = member.property.name.as_str();
            if let Some(method_id) = otter_bytecode::method_id::JsonMethod::from_str(method_name) {
                let arg_regs = compile_call_args(cx, &call.arguments, span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(method_id.as_u32()));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::JsonCall, operands, span);
                return Ok(dst);
            };
        }
        // `Promise.<name>(args)` — typed dispatch via [`PromiseMethod`].
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Promise"
        {
            let method_name = member.property.name.as_str();
            let Some(method_id) = otter_bytecode::method_id::PromiseMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Promise.{method_name}"),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::PromiseCall, operands, span);
            return Ok(dst);
        }
        // `Temporal.<Class>.<method>(args)` — typed dispatch via
        // [`TemporalClassId`] + [`TemporalMethod`]. The callee is a
        // nested static-member expression (`Temporal.<Class>` then
        // `.<method>`), detected directly so the runtime needs no
        // real `Temporal` global.
        if let Expression::StaticMemberExpression(outer) = &member.object
            && let Expression::Identifier(id) = &outer.object
            && id.name.as_str() == "Temporal"
        {
            let class_name = outer.property.name.as_str();
            let method_name = member.property.name.as_str();
            let Some(class_id) = otter_bytecode::method_id::TemporalClassId::from_str(class_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Temporal.{class_name}"),
                    span,
                });
            };
            let Some(method_id) = otter_bytecode::method_id::TemporalMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Temporal.{class_name}.{method_name}"),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(class_id.as_u32()));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::TemporalCall, operands, span);
            return Ok(dst);
        }
        // `Symbol.<method>(args)` — typed dispatch via [`SymbolMethod`].
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Symbol"
        {
            let method_name = member.property.name.as_str();
            let Some(method_id) = otter_bytecode::method_id::SymbolMethod::from_str(method_name)
            else {
                return Err(CompileError::Unsupported {
                    node: format!("Symbol.{method_name}"),
                    span,
                });
            };
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::SymbolCall, operands, span);
            return Ok(dst);
        }
        // §21.1.2 Number static surface — `Number.parseInt` /
        // `Number.parseFloat` are aliases of the global functions
        // (§21.1.2.13 / §21.1.2.12); `Number.isNaN` /
        // `Number.isFinite` / `Number.isInteger` /
        // `Number.isSafeInteger` are the strict variants. All four
        // shapes route through the same `Op::GlobalCall` entry —
        // strict variants pass a distinguishing key so
        // `global_functions::call` can branch.
        // <https://tc39.es/ecma262/#sec-properties-of-the-number-constructor>
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Number"
            && cx.lookup_binding("Number").is_none()
            && find_module_import_binding(cx, "Number").is_none()
        {
            use otter_bytecode::method_id::GlobalMethod;
            let method = member.property.name.as_str();
            let method_id = match method {
                "parseInt" => Some(GlobalMethod::ParseInt),
                "parseFloat" => Some(GlobalMethod::ParseFloat),
                "isNaN" => Some(GlobalMethod::NumberIsNaN),
                "isFinite" => Some(GlobalMethod::NumberIsFinite),
                "isInteger" => Some(GlobalMethod::NumberIsInteger),
                "isSafeInteger" => Some(GlobalMethod::NumberIsSafeInteger),
                _ => None,
            };
            if let Some(method_id) = method_id {
                let arg_regs = compile_call_args(cx, &call.arguments, span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(method_id.as_u32()));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.into_iter().map(Operand::Register));
                cx.emit(Op::GlobalCall, operands, span);
                return Ok(dst);
            }
        }
    }
    // Bare `Symbol(desc)` — fresh primitive symbol per call.
    // Lowers through `Op::SymbolCall` with [`SymbolMethod::Construct`].
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Symbol"
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(
            otter_bytecode::method_id::SymbolMethod::Construct.as_u32(),
        ));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::SymbolCall, operands, span);
        return Ok(dst);
    }
    // §20.3.1 `Boolean(value)` — coerces to boolean. The foundation
    // ships primitive-only Booleans (no wrapper object), so the
    // bare-call form is identical to `!!value`.
    // <https://tc39.es/ecma262/#sec-boolean-constructor>
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Boolean"
        && cx.lookup_binding("Boolean").is_none()
        && find_module_import_binding(cx, "Boolean").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        match arg_regs.first().copied() {
            Some(src) => {
                cx.emit(
                    Op::ToBoolean,
                    vec![Operand::Register(dst), Operand::Register(src)],
                    span,
                );
            }
            None => {
                cx.emit(Op::LoadFalse, vec![Operand::Register(dst)], span);
            }
        }
        return Ok(dst);
    }
    // §22.1.1 / §22.1.2 String constructor + statics. Typed
    // dispatch via [`StringMethod`].
    // <https://tc39.es/ecma262/#sec-string-constructor>
    if let Expression::StaticMemberExpression(member) = callee
        && let Expression::Identifier(id) = &member.object
        && id.name.as_str() == "String"
        && cx.lookup_binding("String").is_none()
        && find_module_import_binding(cx, "String").is_none()
    {
        let method = member.property.name.as_str();
        if let Some(method_id) = otter_bytecode::method_id::StringMethod::from_str(method) {
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::StringCall, operands, span);
            return Ok(dst);
        }
    }
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "String"
        && cx.lookup_binding("String").is_none()
        && find_module_import_binding(cx, "String").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(
            otter_bytecode::method_id::StringMethod::Construct.as_u32(),
        ));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::StringCall, operands, span);
        return Ok(dst);
    }
    // §21.1.1 `Number(value)` — coerce to a primitive number per
    // ToNumber. Bare-call form (no `new`) yields the primitive
    // result; foundation hands it back via `Op::ToNumber`.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Number"
        && cx.lookup_binding("Number").is_none()
        && find_module_import_binding(cx, "Number").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        match arg_regs.first().copied() {
            Some(src) => cx.emit(
                Op::ToNumber,
                vec![Operand::Register(dst), Operand::Register(src)],
                span,
            ),
            None => cx.emit(
                Op::LoadInt32,
                vec![Operand::Register(dst), Operand::Imm32(0)],
                span,
            ),
        }
        return Ok(dst);
    }
    // §20.3.1 `Boolean(value)` — primitive ToBoolean.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Boolean"
        && cx.lookup_binding("Boolean").is_none()
        && find_module_import_binding(cx, "Boolean").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        match arg_regs.first().copied() {
            Some(src) => cx.emit(
                Op::ToBoolean,
                vec![Operand::Register(dst), Operand::Register(src)],
                span,
            ),
            None => cx.emit(Op::LoadFalse, vec![Operand::Register(dst)], span),
        }
        return Ok(dst);
    }
    // §23.1.1.1 `Array(...)` — bare-call form has the same spec
    // body as `new Array(...)`. Both lower to [`Op::ArrayConstruct`]
    // so the single-numeric-length form produces a sparse array.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Array"
        && cx.lookup_binding("Array").is_none()
        && find_module_import_binding(cx, "Array").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::ArrayConstruct, operands, span);
        return Ok(dst);
    }
    // §20.1.1 `Object(value)` — bare-call form. Foundation routes
    // through `Op::ObjectCall("from", ...)` (a helper that wraps
    // the value via the existing object-coercion path). For
    // `undefined` / `null` / no-args, the runtime returns a fresh
    // empty object.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Object"
        && cx.lookup_binding("Object").is_none()
        && find_module_import_binding(cx, "Object").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        // Foundation: when no args, return a fresh object via NewObject.
        if arg_regs.is_empty() {
            let dst = cx.alloc_scratch();
            cx.emit(Op::NewObject, vec![Operand::Register(dst)], span);
            return Ok(dst);
        }
        // With one arg, return the arg unchanged when it's an
        // object (foundation simplification of §20.1.1.1 OrdinaryToPrimitive).
        let dst = arg_regs[0];
        return Ok(dst);
    }
    // §21.4.3 Date statics — typed dispatch via [`DateMethod`].
    // <https://tc39.es/ecma262/#sec-properties-of-the-date-constructor>
    if let Expression::StaticMemberExpression(member) = callee
        && let Expression::Identifier(id) = &member.object
        && id.name.as_str() == "Date"
        && cx.lookup_binding("Date").is_none()
        && find_module_import_binding(cx, "Date").is_none()
    {
        let method_name = member.property.name.as_str();
        if let Some(method_id) = otter_bytecode::method_id::DateMethod::from_str(method_name) {
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::DateCall, operands, span);
            return Ok(dst);
        };
    }
    // §21.2.1 BigInt(value) / §21.2.2 BigInt.<name>(args). Typed
    // dispatch via [`BigIntMethod`].
    // <https://tc39.es/ecma262/#sec-bigint-constructor>
    if let Expression::StaticMemberExpression(member) = callee
        && let Expression::Identifier(id) = &member.object
        && id.name.as_str() == "BigInt"
        && cx.lookup_binding("BigInt").is_none()
        && find_module_import_binding(cx, "BigInt").is_none()
    {
        let method_name = member.property.name.as_str();
        let Some(method_id) = otter_bytecode::method_id::BigIntMethod::from_str(method_name) else {
            return Err(CompileError::Unsupported {
                node: format!("BigInt.{method_name}"),
                span,
            });
        };
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(method_id.as_u32()));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::BigIntCall, operands, span);
        return Ok(dst);
    }
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "BigInt"
        && cx.lookup_binding("BigInt").is_none()
        && find_module_import_binding(cx, "BigInt").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(
            otter_bytecode::method_id::BigIntMethod::Construct.as_u32(),
        ));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::BigIntCall, operands, span);
        return Ok(dst);
    }
    // §20.2.1.1 — bare `Function(arg0, …, body)` is the same as
    // `new Function(...)` per spec; lower both shapes through one
    // path.
    // <https://tc39.es/ecma262/#sec-function-p1-p2-pn-body>
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Function"
        && cx.lookup_binding("Function").is_none()
        && find_module_import_binding(cx, "Function").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::NewFunction, operands, span);
        return Ok(dst);
    }
    // §19.4.1 `eval(source)` — bare-identifier interception.
    // Foundation ships indirect-eval semantics (fresh global
    // scope) which keeps the implementation tractable while
    // covering the common use case of running source-string
    // payloads at runtime.
    // <https://tc39.es/ecma262/#sec-eval-x>
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "eval"
        && cx.lookup_binding("eval").is_none()
        && find_module_import_binding(cx, "eval").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        if arg_regs.is_empty() {
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
            return Ok(dst);
        }
        let src_reg = arg_regs[0];
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::Eval,
            vec![Operand::Register(dst), Operand::Register(src_reg)],
            span,
        );
        return Ok(dst);
    }
    // §19.2 global-function interceptions: route bare-identifier
    // calls like `parseInt(...)` / `isNaN(x)` /
    // `encodeURIComponent(s)` through a single `Op::GlobalCall`
    // dispatcher. The user can shadow these names with a local
    // binding; `lookup_binding` is consulted first so the shadow
    // wins.
    // <https://tc39.es/ecma262/#sec-function-properties-of-the-global-object>
    if let Expression::Identifier(id) = callee {
        let name = id.name.as_str();
        if let Some(method_id) = otter_bytecode::method_id::GlobalMethod::from_str(name)
            && !matches!(method_id.name(), n if n.starts_with("Number."))
            && cx.lookup_binding(name).is_none()
            && find_module_import_binding(cx, name).is_none()
        {
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::GlobalCall, operands, span);
            return Ok(dst);
        }
    }
    // Bare-identifier interceptions — `queueMicrotask(fn, ...args)`
    // is the only one today. Lives at the call-site layer (not
    // inside the StaticMember branch) because the syntax is a
    // direct call, not a method call.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "queueMicrotask"
    {
        // Compile arguments first so any side effects in the args
        // run before the enqueue, matching JS evaluation order.
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        if arg_regs.is_empty() {
            return Err(CompileError::Unsupported {
                node: "queueMicrotask requires a callback argument".to_string(),
                span,
            });
        }
        let mut iter = arg_regs.into_iter();
        let callee_reg = iter.next().expect("checked non-empty");
        let trailing: Vec<u16> = iter.collect();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + trailing.len());
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::ConstIndex(trailing.len() as u32));
        operands.extend(trailing.into_iter().map(Operand::Register));
        cx.emit(Op::QueueMicrotask, operands, span);
        // queueMicrotask returns `undefined` synchronously.
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
        return Ok(dst);
    }
    if let Expression::StaticMemberExpression(member) = callee {
        let method_name = member.property.name.as_str();
        if let Some(dst) =
            try_compile_function_method(cx, &member.object, method_name, &call.arguments, span)?
        {
            return Ok(dst);
        }
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        let name_idx = cx.intern_string_constant(method_name);
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(name_idx));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallMethodValue, operands, span);
        return Ok(dst);
    }
    if let Expression::PrivateFieldExpression(member) = callee {
        let mangled =
            cx.mangle_private(member.field.name.as_str())
                .ok_or(CompileError::Unsupported {
                    node: "PrivateFieldExpression call outside any class body".to_string(),
                    span,
                })?;
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        let name_idx = cx.intern_string_constant(&mangled);
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(name_idx));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallMethodValue, operands, span);
        return Ok(dst);
    }
    // `obj[expr](args...)` — computed-member call. Lower as
    // `LoadElement` + `CallWithThis` so the callee receives the
    // receiver as its `this` value, matching ECMA-262 §13.3.6.1
    // EvaluateCall step 5.b.
    if let Expression::ComputedMemberExpression(member) = callee {
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        let idx_reg = compile_expr(cx, &member.expression, span)?;
        let callee_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadElement,
            vec![
                Operand::Register(callee_reg),
                Operand::Register(receiver_reg),
                Operand::Register(idx_reg),
            ],
            span,
        );
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
        return Ok(dst);
    }
    // Free call: `callee(args...)`.
    let callee_reg = compile_expr(cx, callee, span)?;
    let arg_regs = compile_call_args(cx, &call.arguments, span)?;
    let dst = cx.alloc_scratch();
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(callee_reg));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::Call, operands, span);
    Ok(dst)
}

/// Lower a call expression whose argument list contains at least
/// one `...spread` element to [`Op::CallSpread`]. Two callee
/// shapes are handled:
///
/// - `obj.method(...args)` — receiver is evaluated once, the spread
///   args become an array, dispatched with `this = obj`.
/// - `callee(...args)` — free call, dispatched with
///   `this = undefined`.
///
/// Mixed spread / non-spread arguments are folded into the same
/// args array so `f(a, ...arr, b)` calls `f(a, ...arr items..., b)`.
fn compile_spread_call(
    cx: &mut Compiler,
    callee: &Expression<'_>,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let (callee_reg, this_reg) = match callee {
        Expression::StaticMemberExpression(member) => {
            let recv = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let method_dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(method_dst),
                    Operand::Register(recv),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            (method_dst, recv)
        }
        other => {
            let r = compile_expr(cx, other, span)?;
            let this_dst = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, vec![Operand::Register(this_dst)], span);
            (r, this_dst)
        }
    };
    let args_reg = compile_spread_call_args(cx, arguments, span)?;
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::CallSpread,
        vec![
            Operand::Register(dst),
            Operand::Register(callee_reg),
            Operand::Register(this_reg),
            Operand::Register(args_reg),
        ],
        span,
    );
    Ok(dst)
}

/// Lower the syntactic shapes `<expr>.call(...)`, `<expr>.apply(...)`,
/// and `<expr>.bind(...)` directly to dedicated opcodes. Returns
/// `None` when `method_name` is not one of the recognised triple,
/// so the caller can fall through to the universal
/// [`Op::CallMethodValue`] path.
///
/// The shape detection is **syntactic**: the receiver expression is
/// evaluated only once, so `getFn().call(t, 1)` invokes `getFn()`
/// exactly once. `apply` uses the fixed-arity [`Op::CallWithThis`]
/// path for array literals and falls back to [`Op::CallSpread`] for
/// dynamic argument arrays so the runtime performs the observable
/// argument-list check.
fn try_compile_function_method(
    cx: &mut Compiler,
    receiver: &Expression<'_>,
    method_name: &str,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<Option<u16>, CompileError> {
    match method_name {
        "call" => {
            let callee_reg = compile_expr(cx, receiver, span)?;
            let arg_regs = compile_call_args(cx, arguments, span)?;
            let mut iter = arg_regs.into_iter();
            let this_reg = match iter.next() {
                Some(r) => r,
                None => {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
                    r
                }
            };
            let forwarded: Vec<u16> = iter.collect();
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + forwarded.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::Register(this_reg));
            operands.push(Operand::ConstIndex(forwarded.len() as u32));
            operands.extend(forwarded.into_iter().map(Operand::Register));
            cx.emit(Op::CallWithThis, operands, span);
            Ok(Some(dst))
        }
        "bind" => {
            let callee_reg = compile_expr(cx, receiver, span)?;
            let arg_regs = compile_call_args(cx, arguments, span)?;
            let mut iter = arg_regs.into_iter();
            let this_reg = match iter.next() {
                Some(r) => r,
                None => {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
                    r
                }
            };
            let bound: Vec<u16> = iter.collect();
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + bound.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::Register(this_reg));
            operands.push(Operand::ConstIndex(bound.len() as u32));
            operands.extend(bound.into_iter().map(Operand::Register));
            cx.emit(Op::BindFunction, operands, span);
            Ok(Some(dst))
        }
        "apply" => {
            // `apply(thisArg, argsArray)` — second argument must
            // be statically an array literal for foundation
            // lowering. The receiver is still evaluated even when
            // we fall back to the universal dispatch path so that
            // observable side-effects keep their position.
            let callee_reg = compile_expr(cx, receiver, span)?;
            let mut args_iter = arguments.iter();
            let this_reg = match args_iter.next() {
                Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
                    return Err(CompileError::Unsupported {
                        node: "Function.prototype.apply: spread thisArg".to_string(),
                        span: (s.span.start, s.span.end),
                    });
                }
                Some(other) => compile_expr(cx, other.to_expression(), span)?,
                None => {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
                    r
                }
            };
            let mut forwarded: Vec<u16> = Vec::new();
            let mut dynamic_args: Option<u16> = None;
            match args_iter.next() {
                None => {}
                Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
                    return Err(CompileError::Unsupported {
                        node: "Function.prototype.apply: spread arg list".to_string(),
                        span: (s.span.start, s.span.end),
                    });
                }
                Some(other) => {
                    let expr = unwrap_ts_expr(other.to_expression());
                    match expr {
                        Expression::ArrayExpression(arr) => {
                            for el in &arr.elements {
                                match el {
                                    oxc_ast::ast::ArrayExpressionElement::SpreadElement(s) => {
                                        return Err(CompileError::Unsupported {
                                            node: "Function.prototype.apply: spread element"
                                                .to_string(),
                                            span: (s.span.start, s.span.end),
                                        });
                                    }
                                    oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                                        let r = cx.alloc_scratch();
                                        cx.emit(
                                            Op::LoadUndefined,
                                            vec![Operand::Register(r)],
                                            span,
                                        );
                                        forwarded.push(r);
                                    }
                                    el_expr => {
                                        forwarded.push(compile_expr(
                                            cx,
                                            el_expr.to_expression(),
                                            span,
                                        )?);
                                    }
                                }
                            }
                        }
                        Expression::NullLiteral(_) => {}
                        Expression::Identifier(id) if id.name.as_str() == "undefined" => {}
                        _ => {
                            dynamic_args = Some(compile_expr(cx, expr, span)?);
                        }
                    }
                }
            }
            if args_iter.next().is_some() {
                return Err(CompileError::Unsupported {
                    node: "Function.prototype.apply: extra arguments".to_string(),
                    span,
                });
            }
            let dst = cx.alloc_scratch();
            if let Some(args_reg) = dynamic_args {
                let name_idx = cx.intern_string_constant("apply");
                cx.emit(
                    Op::CallMethodValue,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(callee_reg),
                        Operand::ConstIndex(name_idx),
                        Operand::ConstIndex(2),
                        Operand::Register(this_reg),
                        Operand::Register(args_reg),
                    ],
                    span,
                );
                return Ok(Some(dst));
            }
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + forwarded.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::Register(this_reg));
            operands.push(Operand::ConstIndex(forwarded.len() as u32));
            operands.extend(forwarded.into_iter().map(Operand::Register));
            cx.emit(Op::CallWithThis, operands, span);
            Ok(Some(dst))
        }
        _ => Ok(None),
    }
}

/// Lower a `class … { … }` declaration or expression into the
/// foundation `ClassConstructor` value. The lowering builds:
///
/// 1. The constructor function (synthesised as an empty body for a
///    base class without an explicit `constructor`, or as
///    `constructor(...args) { super(...args); }` for a derived
///    class without one).
/// 2. The instance-side prototype object (`C.prototype`). Each
///    non-static method is installed here; for `extends C`, this
///    object's `[[Prototype]]` chains to `C.prototype`.
/// 3. The static-side object. Each `static` method is installed
///    here; for `extends C`, this object's `[[Prototype]]` chains
///    to the parent's static side so static inheritance falls out
///    of the existing prototype walker.
/// 4. A [`Op::MakeClass`] that fuses constructor / prototype /
///    statics into a single `Value::ClassConstructor`.
///
/// Method bodies that reference `super` resolve through two
/// synthetic upvalues installed in the class scope:
/// `__class_home` (the prototype object methods belong to) and
/// `__class_super` (the parent class value, only present when the
/// class has an `extends` clause).
fn compile_class(
    cx: &mut Compiler,
    class: &oxc_ast::ast::Class<'_>,
    class_name: Option<&str>,
) -> Result<u16, CompileError> {
    let span = (class.span.start, class.span.end);

    // Reject features explicitly out of scope for the foundation
    // slice. Surface clear diagnostics so callers can tell what's
    // not supported yet.
    if !class.decorators.is_empty() {
        return Err(CompileError::Unsupported {
            node: "ClassDeclaration: decorators".to_string(),
            span,
        });
    }
    if class.r#abstract {
        return Err(CompileError::Unsupported {
            node: "ClassDeclaration: abstract".to_string(),
            span,
        });
    }
    if class.declare {
        // Pure type-level declaration — emit nothing observable
        // and hand the caller a `Value::Undefined` register.
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
        return Ok(dst);
    }

    cx.enter_scope();

    // Allocate a fresh private-field namespace and push it on the
    // compiler's class-context stack so every `#name` reference
    // inside the class body mangles into this class's slot.
    let private_namespace = {
        let module = Rc::clone(&cx.top_mut().module);
        let mut m = module.borrow_mut();
        let id = m.next_private_namespace;
        m.next_private_namespace = id.checked_add(1).expect("private-namespace overflow");
        id
    };
    cx.private_namespaces.push(private_namespace);

    // Evaluate the parent class first so observable side-effects
    // happen exactly once per declaration, in source order.
    let super_reg = match &class.super_class {
        Some(expr) => Some(compile_expr(cx, expr, span)?),
        None => None,
    };

    // Build the prototype object up-front so methods can be
    // installed on it as we walk the class body. For `extends`,
    // chain `C.prototype` from the parent's prototype.
    let prototype_reg = cx.alloc_scratch();
    cx.emit(Op::NewObject, vec![Operand::Register(prototype_reg)], span);
    if let Some(parent_reg) = super_reg {
        let parent_proto = cx.alloc_scratch();
        let proto_const = cx.intern_string_constant("prototype");
        cx.emit(
            Op::LoadProperty,
            vec![
                Operand::Register(parent_proto),
                Operand::Register(parent_reg),
                Operand::ConstIndex(proto_const),
            ],
            span,
        );
        cx.emit(
            Op::SetPrototype,
            vec![
                Operand::Register(prototype_reg),
                Operand::Register(parent_proto),
            ],
            span,
        );
    }

    // Statics object — own static methods live here and chain to
    // the parent's statics for `extends`.
    let statics_reg = cx.alloc_scratch();
    cx.emit(Op::NewObject, vec![Operand::Register(statics_reg)], span);
    if let Some(parent_reg) = super_reg {
        cx.emit(
            Op::SetPrototype,
            vec![
                Operand::Register(statics_reg),
                Operand::Register(parent_reg),
            ],
            span,
        );
    }

    // Install the synthetic `__class_home` / `__class_super`
    // captured bindings so method bodies can resolve `super`
    // through the standard upvalue walker.
    let home_storage = cx.declare_captured_binding(SUPER_HOME_NAME, true, span)?;
    cx.emit_store_storage(prototype_reg, home_storage, span);
    cx.mark_initialized(SUPER_HOME_NAME);
    if let Some(parent_reg) = super_reg {
        let super_storage = cx.declare_captured_binding(SUPER_CTOR_NAME, true, span)?;
        cx.emit_store_storage(parent_reg, super_storage, span);
        cx.mark_initialized(SUPER_CTOR_NAME);
    }

    // Find the user-written constructor (if any) and the body's
    // method members. Reject features outside the foundation
    // subset early so the diagnostics are precise.
    let mut ctor_method: Option<&oxc_ast::ast::MethodDefinition<'_>> = None;
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::MethodDefinition(m) => {
                if matches!(m.kind, oxc_ast::ast::MethodDefinitionKind::Constructor) {
                    if ctor_method.is_some() {
                        return Err(CompileError::Unsupported {
                            node: "ClassDeclaration: multiple constructors".to_string(),
                            span: (m.span.start, m.span.end),
                        });
                    }
                    ctor_method = Some(m);
                }
                // Foundation: getters / setters / computed keys all
                // round-trip as plain data methods on the install
                // pass below. Real accessor descriptors land with
                // the §15.7 class-element installer follow-up; for
                // the test262 sweep we accept the syntax so the
                // class declaration compiles.
            }
            oxc_ast::ast::ClassElement::PropertyDefinition(p) => {
                // §15.7 ClassFieldDefinition. The foundation
                // accepts public instance fields and public static
                // fields; private (`#name`) and decorated fields
                // are filed. Computed keys round-trip through the
                // runtime via `Op::StoreElement` in the field
                // installer below.
                if p.declare {
                    continue;
                }
                if !p.decorators.is_empty() {
                    return Err(CompileError::Unsupported {
                        node: "ClassDeclaration: decorated field".to_string(),
                        span: (p.span.start, p.span.end),
                    });
                }
                if !p.r#static {}
            }
            oxc_ast::ast::ClassElement::AccessorProperty(_) => {
                // §15.7 AccessorProperty — degrade to a plain data
                // property with `undefined` initialiser. Tests that
                // rely on accessor semantics will fail; tests that
                // only depend on the syntactic surface keep
                // compiling.
            }
            oxc_ast::ast::ClassElement::StaticBlock(_) => {
                // Allowed — runs at class-declaration time after
                // static fields. See compile_static_block below.
            }
            oxc_ast::ast::ClassElement::TSIndexSignature(_) => {
                // TypeScript-only — erase silently.
            }
        }
    }
    // Collect the instance-field initialisers (in source order) so
    // both user-written and synthetic constructors can prepend them
    // to the body. §15.7.10 InitializeInstanceElements.
    let instance_fields: Vec<&oxc_ast::ast::PropertyDefinition<'_>> = class
        .body
        .body
        .iter()
        .filter_map(|el| match el {
            oxc_ast::ast::ClassElement::PropertyDefinition(p) if !p.r#static && !p.declare => {
                Some(&**p)
            }
            _ => None,
        })
        .collect();

    // Compile the constructor body. When the user didn't write one,
    // synthesize the spec defaults: a base class gets an empty body,
    // a derived class gets `constructor(...args) { super(...args); }`.
    let display_name = class_name.unwrap_or("<class>").to_string();
    let is_derived = super_reg.is_some();
    let (ctor_id, ctor_captures) = match ctor_method {
        Some(m) => compile_class_constructor(
            cx,
            &display_name,
            &m.value.params,
            &m.value.body,
            (m.span.start, m.span.end),
            m.value.r#async,
            &instance_fields,
            is_derived,
        )?,
        None => {
            compile_synthetic_constructor(cx, &display_name, is_derived, span, &instance_fields)?
        }
    };

    let ctor_const = cx.intern_function_id(ctor_id);
    let ctor_reg = cx.alloc_scratch();
    emit_make_callable(cx, ctor_reg, ctor_const, &ctor_captures, false, span);

    // Per §10.2.1.4 ClassDefinitionEvaluation step 24, the class
    // binding becomes initialised *before* the static elements run
    // so they can reference it (e.g., `static x = C.someStatic`).
    // The binding's final value (`MakeClass`) lands at the end of
    // this function — for the early-bind we use the statics object
    // as a stand-in: static initialisers usually reach the class
    // for its statics anyway, and the foundation overwrites with
    // the full class value before any user code outside the class
    // body can observe it.
    if let Some(name) = class_name
        && let Some(info) = cx.lookup_binding(name)
    {
        cx.emit_store_storage(statics_reg, info.storage, span);
        cx.mark_initialized(name);
    }

    // Install methods (instance + static) onto the right side.
    // Foundation: getter / setter accessors round-trip as plain
    // data methods (their function body is callable and addressable
    // by name; accessor [[Get]] / [[Set]] semantics await the
    // §15.7 class-element installer follow-up). Computed keys
    // resolve at runtime via `Op::StoreElement`.
    for element in &class.body.body {
        let oxc_ast::ast::ClassElement::MethodDefinition(m) = element else {
            continue;
        };
        if matches!(m.kind, oxc_ast::ast::MethodDefinitionKind::Constructor) {
            continue;
        }
        let method_span = (m.span.start, m.span.end);
        let target_reg = if m.r#static {
            statics_reg
        } else {
            prototype_reg
        };
        // Compute the static name (when known) for diagnostics +
        // the method's `.name` intrinsic.
        let static_name: Option<String> = if !m.computed {
            match &m.key {
                oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                    Some(id.name.as_str().to_string())
                }
                oxc_ast::ast::PropertyKey::StringLiteral(lit) => Some(lit.value.to_string()),
                oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => Some(
                    cx.mangle_private(pid.name.as_str())
                        .ok_or(CompileError::Unsupported {
                            node: "ClassDeclaration: private method outside class".to_string(),
                            span: method_span,
                        })?,
                ),
                oxc_ast::ast::PropertyKey::NumericLiteral(lit) => Some(lit.value.to_string()),
                _ => None,
            }
        } else {
            None
        };
        let body_name = static_name
            .clone()
            .unwrap_or_else(|| "<computed>".to_string());
        let (m_id, m_captures) = compile_function_full(
            cx,
            &body_name,
            &m.value.params,
            &m.value.body,
            method_span,
            m.value.r#async,
            m.value.generator,
            true,
        )?;
        let m_const = cx.intern_function_id(m_id);
        let m_reg = cx.alloc_scratch();
        emit_make_callable(cx, m_reg, m_const, &m_captures, false, method_span);
        match (&static_name, m.computed) {
            (Some(name), false) => {
                let name_const = cx.intern_string_constant(name);
                let store_scratch = cx.alloc_scratch();
                cx.emit(
                    Op::StoreProperty,
                    vec![
                        Operand::Register(target_reg),
                        Operand::ConstIndex(name_const),
                        Operand::Register(m_reg),
                        Operand::Register(store_scratch),
                    ],
                    method_span,
                );
            }
            _ => {
                // Computed key (or unsupported key kind) — evaluate
                // at runtime and write via Op::StoreElement.
                let key_expr = m
                    .key
                    .as_expression()
                    .ok_or_else(|| CompileError::Unsupported {
                        node: "ClassDeclaration: non-expression computed key".to_string(),
                        span: method_span,
                    })?;
                let key_reg = compile_expr(cx, key_expr, method_span)?;
                cx.emit_store_element(target_reg, key_reg, m_reg, method_span);
            }
        }
    }

    // §15.7.10 InitializeStaticElements — walk the body in source
    // order, evaluating static fields and static-init blocks
    // against the statics object.
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::PropertyDefinition(p) if p.r#static && !p.declare => {
                let pspan = (p.span.start, p.span.end);
                let value_reg = match &p.value {
                    Some(expr) => compile_expr(cx, expr, pspan)?,
                    None => {
                        let dst = cx.alloc_scratch();
                        cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], pspan);
                        dst
                    }
                };
                if p.computed {
                    let key_expr =
                        p.key
                            .as_expression()
                            .ok_or_else(|| CompileError::Unsupported {
                                node: "ClassDeclaration: non-expression computed static field key"
                                    .to_string(),
                                span: pspan,
                            })?;
                    let key_reg = compile_expr(cx, key_expr, pspan)?;
                    cx.emit_store_element(statics_reg, key_reg, value_reg, pspan);
                } else {
                    let key_str = match &p.key {
                        oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                            id.name.as_str().to_string()
                        }
                        oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
                        oxc_ast::ast::PropertyKey::NumericLiteral(lit) => lit.value.to_string(),
                        oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => cx
                            .mangle_private(pid.name.as_str())
                            .ok_or(CompileError::Unsupported {
                                node: "ClassDeclaration: private static field outside class"
                                    .to_string(),
                                span: pspan,
                            })?,
                        _ => {
                            return Err(CompileError::Unsupported {
                                node: "ClassDeclaration: non-string static field key".to_string(),
                                span: pspan,
                            });
                        }
                    };
                    cx.emit_store_property(statics_reg, &key_str, value_reg, pspan);
                }
            }
            oxc_ast::ast::ClassElement::StaticBlock(s) => {
                // §15.7.4 StaticBlock — a synthesised function with
                // no params; `this` bound to the statics object.
                // Compile a closure-less function from the block
                // body, then invoke it with the statics object as
                // the receiver via `Op::CallWithThis`.
                let bspan = (s.span.start, s.span.end);
                let function_id = compile_static_block(cx, &display_name, &s.body, bspan)?;
                let const_idx = cx.intern_function_id(function_id);
                let fn_reg = cx.alloc_scratch();
                cx.emit(
                    Op::MakeFunction,
                    vec![Operand::Register(fn_reg), Operand::ConstIndex(const_idx)],
                    bspan,
                );
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::CallWithThis,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(fn_reg),
                        Operand::Register(statics_reg),
                        Operand::ConstIndex(0),
                    ],
                    bspan,
                );
            }
            _ => {}
        }
    }

    let class_reg = cx.alloc_scratch();
    cx.emit(
        Op::MakeClass,
        vec![
            Operand::Register(class_reg),
            Operand::Register(ctor_reg),
            Operand::Register(prototype_reg),
            Operand::Register(statics_reg),
        ],
        span,
    );

    cx.private_namespaces.pop();
    cx.exit_scope();
    Ok(class_reg)
}

/// Build a synthetic constructor function body for classes that
/// don't declare their own. Foundation rules:
///
/// - Base class: empty body, no params.
/// - Derived class: `(…args) => super(...args)` lowered through the
///   normal compiler path so `super` resolves via the same upvalue
///   capture as user-written constructors.
fn compile_synthetic_constructor(
    parent: &mut Compiler,
    name: &str,
    is_derived: bool,
    span: (u32, u32),
    instance_fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
) -> Result<(u32, Vec<u32>), CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let child = FunctionContext::new(Rc::clone(&module)).with_strict(true);
    // No body to pre-pass; only the synthesised super call needs
    // outer captures.
    parent.push(child);
    parent.enter_scope();

    // Reserve the function record up-front so the slot id is
    // stable across recursive compile cycles.
    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        is_strict: true,
        ..Default::default()
    });

    if is_derived {
        // Default derived ctor is `constructor(...args) { super(...args); }`.
        // We have no user-visible `args` parameter (the body uses
        // the receiver's already-allocated `this`), so simulate by
        // pulling the captured `__class_super` directly and
        // forwarding zero args. A faithful spec-default would
        // forward `arguments`, but the foundation has no
        // `arguments` object yet — `super()` with no args is
        // sufficient for the common chained-base-init case.
        let super_ctor = load_synthetic_capture(parent, SUPER_CTOR_NAME, span)?;
        let this_reg = parent.alloc_scratch();
        parent.emit(Op::LoadThis, vec![Operand::Register(this_reg)], span);
        let dst = parent.alloc_scratch();
        parent.emit(
            Op::CallWithThis,
            vec![
                Operand::Register(dst),
                Operand::Register(super_ctor),
                Operand::Register(this_reg),
                Operand::ConstIndex(0),
            ],
            span,
        );
    }
    // §15.7.10 InitializeInstanceElements — run instance-field
    // initialisers with `this` bound to the new instance, after the
    // super() call has run.
    emit_instance_field_inits(parent, instance_fields)?;
    parent.emit(Op::ReturnUndefined, vec![], span);

    parent.exit_scope();
    let child = parent.pop();
    let captures = child.parent_captures.clone();
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved synthetic ctor slot");
    slot.locals = 0;
    slot.scratch = child.scratch;
    slot.param_count = 0;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}

/// Compile a user-written class constructor, prepending instance-
/// field initialisers when present. For base classes the fields run
/// at the top of the body; for derived classes the foundation
/// rejects this combination upstream so we never get here.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
/// - <https://tc39.es/ecma262/#sec-initializeinstanceelements>
#[allow(clippy::too_many_arguments)]
fn compile_class_constructor(
    parent: &mut Compiler,
    name: &str,
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &Option<oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody<'_>>>,
    span: (u32, u32),
    is_async: bool,
    instance_fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
    is_derived: bool,
) -> Result<(u32, Vec<u32>), CompileError> {
    if instance_fields.is_empty() {
        return compile_function_full(parent, name, params, body, span, is_async, false, true);
    }
    // Compile the function with field-init injection. We mirror
    // `compile_function` but inject the field stores after the
    // self-name binding and before the user body. The compiler
    // doesn't have a public hook for this, so we duplicate the
    // setup here.
    let module = Rc::clone(&parent.top_mut().module);
    validate_formal_parameter_names(params, true, false, span)?;
    let mut child = FunctionContext::new(Rc::clone(&module)).with_strict(true);
    if let Some(b) = body {
        child.captured_names = capture::analyze_function(Some(params), b);
    }
    parent.push(child);
    parent.enter_scope();

    let param_count = u16::try_from(params.items.len()).expect("too many parameters");
    parent.scratch = param_count;
    let has_rest = params.rest.is_some();

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        is_strict: true,
        ..Default::default()
    });

    for (ordinal, param) in params.items.iter().enumerate() {
        compile_formal_parameter(
            parent,
            ordinal as u16,
            &param.pattern,
            param.initializer.as_deref(),
            span,
            false,
        )?;
    }
    if let Some(rest) = &params.rest {
        compile_rest_parameter(parent, &rest.rest.argument, span)?;
    }

    let self_storage = parent.declare_binding(name, false, span)?;
    let const_idx = parent.intern_function_id(function_id);
    let tmp = parent.alloc_scratch();
    parent.emit(
        Op::MakeFunction,
        vec![Operand::Register(tmp), Operand::ConstIndex(const_idx)],
        span,
    );
    parent.emit_store_storage(tmp, self_storage, span);
    parent.mark_initialized(name);

    // §15.7.10 InitializeInstanceElements — base classes run field
    // initialisers immediately (before the user body); derived
    // classes run them right after the user-written `super(...)`
    // call returns, so `this` is already allocated.
    if !is_derived {
        emit_instance_field_inits(parent, instance_fields)?;
    }

    if let Some(body) = body {
        let mut var_names: Vec<String> = Vec::new();
        hoist_var_names(&body.statements, &mut var_names);
        pre_declare_var_bindings(parent, &var_names, span)?;
        let mut lex_names: Vec<(String, bool)> = Vec::new();
        hoist_lexical_names(&body.statements, &mut lex_names);
        pre_declare_lexical_bindings(parent, &lex_names, span)?;
        hoist_function_declarations(parent, &body.statements)?;
        let mut fields_emitted = !is_derived;
        for stmt in &body.statements {
            compile_statement(parent, stmt)?;
            // Inject the field initialisers as soon as the user's
            // first statement-level `super(...)` call has run. This
            // mirrors the spec's "after the super call returns" rule
            // for derived constructors. If the user doesn't write a
            // top-level super-call (defensive shape) we fall through
            // to the post-body emission below.
            if !fields_emitted && is_top_level_super_call(stmt) {
                emit_instance_field_inits(parent, instance_fields)?;
                fields_emitted = true;
            }
        }
        if !fields_emitted {
            emit_instance_field_inits(parent, instance_fields)?;
        }
    } else if is_derived {
        // No body at all (degenerate shape) — emit field inits.
        emit_instance_field_inits(parent, instance_fields)?;
    }
    parent.exit_scope();
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
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok((function_id, captures))
}

/// Emit `this.<key> = <init>` for each instance-field declaration.
/// The initializer expression is evaluated in the current scope
/// (the constructor's scope, with access to params + outer
/// upvalues) per §15.7.10 InitializeFieldsForReceiver.
fn emit_instance_field_inits(
    cx: &mut Compiler,
    fields: &[&oxc_ast::ast::PropertyDefinition<'_>],
) -> Result<(), CompileError> {
    for p in fields {
        let pspan = (p.span.start, p.span.end);
        let value_reg = match &p.value {
            Some(expr) => compile_expr(cx, expr, pspan)?,
            None => {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], pspan);
                dst
            }
        };
        let this_reg = cx.alloc_scratch();
        cx.emit(Op::LoadThis, vec![Operand::Register(this_reg)], pspan);
        if p.computed {
            // §15.7.10 — computed-key field. Evaluate the key
            // expression at constructor-run time and write via
            // `Op::StoreElement`.
            let key_expr = p
                .key
                .as_expression()
                .ok_or_else(|| CompileError::Unsupported {
                    node: "ClassDeclaration: non-expression computed instance field key"
                        .to_string(),
                    span: pspan,
                })?;
            let key_reg = compile_expr(cx, key_expr, pspan)?;
            cx.emit_store_element(this_reg, key_reg, value_reg, pspan);
            continue;
        }
        let key_str = match &p.key {
            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::NumericLiteral(lit) => lit.value.to_string(),
            oxc_ast::ast::PropertyKey::PrivateIdentifier(pid) => cx
                .mangle_private(pid.name.as_str())
                .ok_or(CompileError::Unsupported {
                    node: "ClassDeclaration: private instance field outside class".to_string(),
                    span: pspan,
                })?,
            _ => {
                return Err(CompileError::Unsupported {
                    node: "ClassDeclaration: non-string instance field key".to_string(),
                    span: pspan,
                });
            }
        };
        cx.emit_store_property(this_reg, &key_str, value_reg, pspan);
    }
    Ok(())
}

/// `true` when `stmt` is `super(...)` at the top level of a
/// derived-class constructor body — the canonical injection point
/// for instance-field initialisers per §15.7.10 step 9.
fn is_top_level_super_call(stmt: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(es) = stmt else {
        return false;
    };
    let Expression::CallExpression(call) = &es.expression else {
        return false;
    };
    matches!(call.callee, Expression::Super(_))
}

/// Compile a `static { … }` block as a synthesised parameterless
/// function. The block's body is treated like a function body for
/// scoping; `this` is bound to the statics object by the call site
/// (`Op::CallWithThis`).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-class-static-block>
fn compile_static_block(
    parent: &mut Compiler,
    class_name: &str,
    body: &oxc_allocator::Vec<'_, Statement<'_>>,
    span: (u32, u32),
) -> Result<u32, CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let child = FunctionContext::new(Rc::clone(&module)).with_strict(true);
    parent.push(child);
    parent.enter_scope();

    let function_id = module.borrow().functions.len() as u32;
    module.borrow_mut().functions.push(Function {
        id: function_id,
        name: format!("{class_name}.<static-init>"),
        span,
        is_strict: true,
        ..Default::default()
    });

    // Same pre-passes as a regular function body — top-level
    // `var` / `let` / `function` statements inside a static block
    // hoist to the block's scope, not the surrounding class.
    let mut var_names: Vec<String> = Vec::new();
    hoist_var_names(body, &mut var_names);
    pre_declare_var_bindings(parent, &var_names, span)?;
    let mut lex_names: Vec<(String, bool)> = Vec::new();
    hoist_lexical_names(body, &mut lex_names);
    pre_declare_lexical_bindings(parent, &lex_names, span)?;
    hoist_function_declarations(parent, body)?;
    for stmt in body {
        compile_statement(parent, stmt)?;
    }
    parent.exit_scope();
    parent.emit(Op::ReturnUndefined, vec![], span);

    let child = parent.pop();
    let mut module_mut = module.borrow_mut();
    let slot = module_mut
        .functions
        .get_mut(function_id as usize)
        .expect("reserved static-block slot");
    slot.locals = 0;
    slot.scratch = child.scratch;
    slot.param_count = 0;
    slot.own_upvalue_count = child.own_upvalue_count;
    slot.code = child.code;
    slot.spans = child.spans;
    Ok(function_id)
}

/// Synthetic name for the per-method "home object" upvalue that
/// the class lowering installs in the enclosing class scope. The
/// value is the prototype object that the method belongs to —
/// `super.x` walks one hop up its `[[Prototype]]` chain to find the
/// parent's binding.
const SUPER_HOME_NAME: &str = "__class_home";

/// Synthetic name for the per-derived-constructor "super
/// constructor" upvalue. Holds the parent class value so
/// `super(args)` knows what to invoke with the current receiver.
const SUPER_CTOR_NAME: &str = "__class_super";

/// Resolve a synthetic captured name (`__class_home` / `__class_super`)
/// into a register holding its current value. Returns
/// [`CompileError::Unsupported`] when the surrounding function has
/// no class context, which is what the user sees on stray `super`
/// usages outside a class body.
fn load_synthetic_capture(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if let Some(info) = cx.lookup_binding(name) {
        let dst = cx.alloc_scratch();
        cx.emit_load_storage(dst, info.storage, span);
        return Ok(dst);
    }
    if let Some(uv_idx) = cx.resolve_capture(name) {
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadUpvalue,
            vec![Operand::Register(dst), Operand::Imm32(uv_idx as i32)],
            span,
        );
        return Ok(dst);
    }
    Err(CompileError::Unsupported {
        node: format!("super used outside a class method (`{name}` not in scope)"),
        span,
    })
}

/// Lower `super(args...)` to a `CallWithThis` against the captured
/// parent constructor with `this = current frame's this`.
fn compile_super_call(
    cx: &mut Compiler,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let super_ctor = load_synthetic_capture(cx, SUPER_CTOR_NAME, span)?;
    let this_reg = cx.alloc_scratch();
    cx.emit(Op::LoadThis, vec![Operand::Register(this_reg)], span);
    let has_spread = arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    let dst = cx.alloc_scratch();
    if has_spread {
        let args_reg = compile_spread_call_args(cx, arguments, span)?;
        cx.emit(
            Op::CallSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(super_ctor),
                Operand::Register(this_reg),
                Operand::Register(args_reg),
            ],
            span,
        );
    } else {
        let arg_regs = compile_call_args(cx, arguments, span)?;
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(super_ctor));
        operands.push(Operand::Register(this_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
    }
    Ok(dst)
}

/// Lower `super.method(args...)` to a parent-prototype lookup
/// followed by a `CallWithThis` against the resolved method with
/// `this = current frame's this`.
fn compile_super_method_call(
    cx: &mut Compiler,
    method_name: &str,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let method_reg = load_super_method(cx, method_name, span)?;
    let this_reg = cx.alloc_scratch();
    cx.emit(Op::LoadThis, vec![Operand::Register(this_reg)], span);
    let has_spread = arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    let dst = cx.alloc_scratch();
    if has_spread {
        let args_reg = compile_spread_call_args(cx, arguments, span)?;
        cx.emit(
            Op::CallSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(method_reg),
                Operand::Register(this_reg),
                Operand::Register(args_reg),
            ],
            span,
        );
    } else {
        let arg_regs = compile_call_args(cx, arguments, span)?;
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(method_reg));
        operands.push(Operand::Register(this_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
    }
    Ok(dst)
}

/// Lower a `super.x` read (no call) to a parent-prototype property
/// load. Resolves to a register holding the looked-up value.
fn compile_super_member_load(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    load_super_method(cx, name, span)
}

/// Shared helper: load `Object.getPrototypeOf(__class_home).<name>`
/// into a fresh register. The compiler emits `GetPrototype` +
/// `LoadProperty` rather than introducing a dedicated opcode — the
/// foundation interpreter doesn't pay for a new dispatch arm and
/// the `super` shape stays bytecode-readable.
fn load_super_method(cx: &mut Compiler, name: &str, span: (u32, u32)) -> Result<u16, CompileError> {
    let home_reg = load_synthetic_capture(cx, SUPER_HOME_NAME, span)?;
    let parent_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetPrototype,
        vec![Operand::Register(parent_reg), Operand::Register(home_reg)],
        span,
    );
    let name_idx = cx.intern_string_constant(name);
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::LoadProperty,
        vec![
            Operand::Register(dst),
            Operand::Register(parent_reg),
            Operand::ConstIndex(name_idx),
        ],
        span,
    );
    Ok(dst)
}

/// `true` when `name` is one of the seven canonical native error
/// classes (`Error`, `TypeError`, `RangeError`, `SyntaxError`,
/// `ReferenceError`, `URIError`, `EvalError`).
///
/// Used by [`compile_expr`] (bare-identifier read) and
/// [`compile_method_call`] / new-expression lowering. Local
/// bindings of the same name take precedence — callers must
/// confirm `lookup_binding` and `find_module_import_binding` both
/// returned `None` before consulting this helper.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
fn is_builtin_error_class_name(name: &str) -> bool {
    matches!(
        name,
        "Error"
            | "TypeError"
            | "RangeError"
            | "SyntaxError"
            | "ReferenceError"
            | "URIError"
            | "EvalError"
            | "AggregateError"
    )
}

/// `true` when the `NewBuiltinError` fast path can lower this
/// call without losing semantics — i.e. when the call only
/// supplies the operand shapes the opcode encodes.
///
/// `new Error(message, options)` / `new TypeError(message,
/// options)` etc. — and `new AggregateError(errors, message,
/// options)` — need the runtime constructor path
/// (§20.5.6.1.1 InstallErrorCause requires reading
/// `options.cause`). Those call shapes fall through to the
/// standard `new` dispatch so the registered native constructor
/// handles the option bag.
fn builtin_error_construct_fast_path_applies(
    kind: &str,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
) -> bool {
    let max_args = if kind == "AggregateError" { 2 } else { 1 };
    arguments.len() <= max_args
}

/// Lower `new <Kind>(arg)` / `<Kind>(arg)` for any of the seven
/// canonical native error classes to [`Op::NewBuiltinError`]. The
/// `Error` kind keeps the legacy [`Op::NewError`] lowering for
/// backwards compatibility with already-shipped fixtures.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
fn compile_builtin_error_construct(
    cx: &mut Compiler,
    kind: &str,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    // §20.5.7.1 — `new AggregateError(errors, message?)` accepts two
    // arguments. The first is the error iterable; the second is the
    // optional message. Lower as `NewBuiltinError(message)` followed
    // by `StoreProperty("errors", errors_arg)`.
    if kind == "AggregateError" {
        if arguments.len() > 2 {
            return Err(CompileError::Unsupported {
                node: format!("{kind}: more than two arguments"),
                span,
            });
        }
        let errors_reg = match arguments.first() {
            None => {
                let r = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
                r
            }
            Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
                return Err(CompileError::Unsupported {
                    node: format!("{kind}: spread argument"),
                    span: (s.span.start, s.span.end),
                });
            }
            Some(other) => compile_expr(cx, other.to_expression(), span)?,
        };
        let msg_reg = match arguments.get(1) {
            None => {
                let r = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
                r
            }
            Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
                return Err(CompileError::Unsupported {
                    node: format!("{kind}: spread argument"),
                    span: (s.span.start, s.span.end),
                });
            }
            Some(other) => compile_expr(cx, other.to_expression(), span)?,
        };
        let dst = cx.alloc_scratch();
        let kind_idx = cx.intern_string_constant(kind);
        cx.emit(
            Op::NewBuiltinError,
            vec![
                Operand::Register(dst),
                Operand::ConstIndex(kind_idx),
                Operand::Register(msg_reg),
            ],
            span,
        );
        // Attach `errors` own property on the freshly built instance.
        let key_idx = cx.intern_string_constant("errors");
        let scratch = cx.alloc_scratch();
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(dst),
                Operand::ConstIndex(key_idx),
                Operand::Register(errors_reg),
                Operand::Register(scratch),
            ],
            span,
        );
        return Ok(dst);
    }
    if arguments.len() > 1 {
        return Err(CompileError::Unsupported {
            node: format!("{kind}: more than one argument (foundation accepts only `message`)"),
            span,
        });
    }
    let msg_reg = match arguments.first() {
        None => {
            let r = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
            r
        }
        Some(oxc_ast::ast::Argument::SpreadElement(s)) => {
            return Err(CompileError::Unsupported {
                node: format!("{kind}: spread argument"),
                span: (s.span.start, s.span.end),
            });
        }
        Some(other) => compile_expr(cx, other.to_expression(), span)?,
    };
    if kind == "Error" {
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::NewError,
            vec![Operand::Register(dst), Operand::Register(msg_reg)],
            span,
        );
        return Ok(dst);
    }
    let dst = cx.alloc_scratch();
    let kind_idx = cx.intern_string_constant(kind);
    cx.emit(
        Op::NewBuiltinError,
        vec![
            Operand::Register(dst),
            Operand::ConstIndex(kind_idx),
            Operand::Register(msg_reg),
        ],
        span,
    );
    Ok(dst)
}

/// Lower a recognised `Object.<method>(args...)` call site to its
/// dedicated opcode. Foundation slice 19 covers `create`,
/// `getPrototypeOf`, and `setPrototypeOf`.
fn compile_object_builtin(
    cx: &mut Compiler,
    method: &str,
    arg_regs: &[u16],
    span: (u32, u32),
) -> Result<u16, CompileError> {
    match (method, arg_regs.len()) {
        ("create", 1) => {
            let proto_reg = arg_regs[0];
            let dst = cx.alloc_scratch();
            cx.emit(Op::NewObject, vec![Operand::Register(dst)], span);
            cx.emit(
                Op::SetPrototype,
                vec![Operand::Register(dst), Operand::Register(proto_reg)],
                span,
            );
            Ok(dst)
        }
        // §20.1.2.2 `Object.create(O, Properties)` — proto + initial
        // descriptor object form. Routes through the runtime's
        // `Object.create` handler so descriptor coercion stays
        // alongside `defineProperties`.
        // <https://tc39.es/ecma262/#sec-object.create>
        ("create", 2) => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::ObjectCall,
                vec![
                    Operand::Register(dst),
                    Operand::ConstIndex(otter_bytecode::method_id::ObjectMethod::Create.as_u32()),
                    Operand::ConstIndex(2),
                    Operand::Register(arg_regs[0]),
                    Operand::Register(arg_regs[1]),
                ],
                span,
            );
            Ok(dst)
        }
        ("getPrototypeOf", 1) => {
            let obj_reg = arg_regs[0];
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::GetPrototype,
                vec![Operand::Register(dst), Operand::Register(obj_reg)],
                span,
            );
            Ok(dst)
        }
        ("setPrototypeOf", 2) => {
            let obj_reg = arg_regs[0];
            let proto_reg = arg_regs[1];
            cx.emit(
                Op::SetPrototype,
                vec![Operand::Register(obj_reg), Operand::Register(proto_reg)],
                span,
            );
            // Spec says `setPrototypeOf` returns `obj`; foundation
            // mirrors that.
            Ok(obj_reg)
        }
        // `Object.is(x, y)` — ECMA-262 §20.1.2.13. Lowers to
        // [`Op::SameValue`], which dispatches §7.2.11 SameValue.
        // <https://tc39.es/ecma262/#sec-object.is>
        ("is", 2) => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::SameValue,
                vec![
                    Operand::Register(dst),
                    Operand::Register(arg_regs[0]),
                    Operand::Register(arg_regs[1]),
                ],
                span,
            );
            Ok(dst)
        }
        // ECMA-262 §20.1.2 / §10.1.6 — Object descriptor surface.
        // Typed dispatch via [`ObjectMethod`].
        // <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
        _ if otter_bytecode::method_id::ObjectMethod::from_str(method).is_some() => {
            let method_id = otter_bytecode::method_id::ObjectMethod::from_str(method)
                .expect("guard above ensures Some");
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(method_id.as_u32()));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.iter().copied().map(Operand::Register));
            cx.emit(Op::ObjectCall, operands, span);
            Ok(dst)
        }
        _ => Err(CompileError::Unsupported {
            node: format!("Object.{method}/{}", arg_regs.len()),
            span,
        }),
    }
}

fn is_compiler_lowered_object_static(method: &str) -> bool {
    matches!(
        method,
        "create" | "getPrototypeOf" | "setPrototypeOf" | "is"
    ) || otter_bytecode::method_id::ObjectMethod::from_str(method).is_some()
}

/// §21.1.1 Number static constants. Returns the IEEE-754 value the
/// compiler inlines via `Op::LoadNumber` when the user reads
/// `Number.<CONST>` outside any local shadow.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-value-properties-of-the-number-constructor>
fn number_static_constant(name: &str) -> Option<f64> {
    Some(match name {
        // §21.1.1.6
        "MAX_SAFE_INTEGER" => 9_007_199_254_740_991.0,
        // §21.1.1.10
        "MIN_SAFE_INTEGER" => -9_007_199_254_740_991.0,
        // §21.1.1.4
        "MAX_VALUE" => f64::MAX,
        // §21.1.1.7 — smallest positive subnormal.
        "MIN_VALUE" => 5e-324,
        // §21.1.1.1 — 2^-52.
        "EPSILON" => f64::EPSILON,
        // §21.1.1.11 / §21.1.1.9
        "POSITIVE_INFINITY" => f64::INFINITY,
        "NEGATIVE_INFINITY" => f64::NEG_INFINITY,
        // §21.1.1.8
        "NaN" => f64::NAN,
        _ => return None,
    })
}

/// §21.3.1 Math value properties. Returns the names the compiler
/// may route through `Op::MathLoad`; method properties must remain
/// ordinary loads so `Math.abs.length` and extracted calls observe
/// the real namespace installed by bootstrap.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-value-properties-of-the-math-object>
fn math_static_constant(name: &str) -> Option<()> {
    match name {
        "E" | "LN10" | "LN2" | "LOG10E" | "LOG2E" | "PI" | "SQRT1_2" | "SQRT2" => Some(()),
        _ => None,
    }
}

/// Lower a template literal `\`hello ${x} world\`` per §13.2.8 — a
/// sequence of `String` concats over cooked quasis and
/// interpolations.
///
/// # Algorithm
/// Per ECMA-262 §13.2.8.6:
/// 1. Evaluate `quasi[0].cooked` → result.
/// 2. For each expression `expr[i]`: `result = result + ToString(expr[i])`.
///    The runtime handles `ToString` via `Op::Add`'s string-or-numeric
///    ladder once `Op::ToPrimitive(default)` ran on each operand —
///    template-literal interpolations always produce strings, so the
///    `+` lowering works out of the box.
/// 3. After each interpolation, append `quasi[i+1].cooked`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-template-literals>
fn intern_template_quasi(cx: &mut Compiler, quasi: &oxc_ast::ast::TemplateElement<'_>) -> u32 {
    let cooked = quasi.value.cooked.as_deref().unwrap_or("");
    if quasi.lone_surrogates {
        cx.intern_utf16_string_constant(decode_lone_surrogate_string(cooked))
    } else {
        cx.intern_string_constant(cooked)
    }
}

fn compile_template_literal(
    cx: &mut Compiler,
    t: &oxc_ast::ast::TemplateLiteral<'_>,
) -> Result<u16, CompileError> {
    let span = (t.span.start, t.span.end);
    if t.expressions.is_empty() && t.quasis.len() == 1 {
        let dst = cx.alloc_scratch();
        let const_idx = intern_template_quasi(cx, &t.quasis[0]);
        cx.emit(
            Op::LoadString,
            vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
            span,
        );
        return Ok(dst);
    }
    // Seed with first cooked quasi.
    let mut acc = {
        let dst = cx.alloc_scratch();
        let const_idx = intern_template_quasi(cx, &t.quasis[0]);
        cx.emit(
            Op::LoadString,
            vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
            span,
        );
        dst
    };
    for (i, expr) in t.expressions.iter().enumerate() {
        let expr_reg = compile_expr(cx, expr, span)?;
        // Mirror the BinaryExpression `+` lowering: pass each operand
        // through ToPrimitive(default) so `Op::Add`'s string-or-
        // numeric ladder fires correctly when an object exposes
        // `[Symbol.toPrimitive]` / `valueOf` / `toString`.
        let lhs_in = emit_to_primitive(cx, acc, "default", span);
        let rhs_in = emit_to_primitive(cx, expr_reg, "default", span);
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::Add,
            vec![
                Operand::Register(dst),
                Operand::Register(lhs_in),
                Operand::Register(rhs_in),
            ],
            span,
        );
        acc = dst;
        // Append the next cooked quasi.
        let next_quasi = &t.quasis[i + 1];
        let cooked = next_quasi.value.cooked.as_deref().unwrap_or("");
        if !cooked.is_empty() {
            let quasi_reg = cx.alloc_scratch();
            let const_idx = intern_template_quasi(cx, next_quasi);
            cx.emit(
                Op::LoadString,
                vec![Operand::Register(quasi_reg), Operand::ConstIndex(const_idx)],
                span,
            );
            let lhs_in = emit_to_primitive(cx, acc, "default", span);
            let rhs_in = emit_to_primitive(cx, quasi_reg, "default", span);
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::Add,
                vec![
                    Operand::Register(dst),
                    Operand::Register(lhs_in),
                    Operand::Register(rhs_in),
                ],
                span,
            );
            acc = dst;
        }
    }
    Ok(acc)
}

/// Lower a tagged-template call: `tag\`...${a}...${b}...\`` per
/// ECMA-262 §13.3.11.4.
///
/// # Algorithm
/// 1. Build the `strings` array — `cooked` quasis in order. Attach
///    a `.raw` own property whose value is an array of the same
///    length holding the raw quasi text.
/// 2. Evaluate every interpolation expression, in source order.
/// 3. Call `tag(strings, ...exprs)` with `this = undefined` (foundation
///    matches the spec's `Reference` resolution; method-receiver
///    forms via `obj.tag\`...\`` are filed as a follow-up).
///
/// `strings.raw` is installed via `Op::StoreProperty` for foundation
/// fidelity; spec mandates the strings array be frozen and the `raw`
/// array be a separate own property — the foundation slice ships
/// the un-frozen shape and files freezing as a follow-up.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-tagged-templates>
/// - <https://tc39.es/ecma262/#sec-runtime-semantics-getemplateobject>
fn compile_tagged_template(
    cx: &mut Compiler,
    t: &oxc_ast::ast::TaggedTemplateExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (t.span.start, t.span.end);

    // §22.1.2.4 String.raw — recognise the literal call shape
    // `String.raw\`...\`` and inline the raw-text reconstruction.
    // Avoids the need for a real `String` namespace binding.
    // <https://tc39.es/ecma262/#sec-string.raw>
    if let Expression::StaticMemberExpression(member) = &t.tag
        && let Expression::Identifier(id) = &member.object
        && id.name.as_str() == "String"
        && member.property.name.as_str() == "raw"
        && cx.lookup_binding("String").is_none()
    {
        return compile_string_raw_template(cx, &t.quasi, span);
    }

    let tag_reg = compile_expr(cx, &t.tag, span)?;

    // Build cooked + raw quasi arrays.
    let mut cooked_regs: Vec<u16> = Vec::with_capacity(t.quasi.quasis.len());
    let mut raw_regs: Vec<u16> = Vec::with_capacity(t.quasi.quasis.len());
    for q in t.quasi.quasis.iter() {
        let cooked = q.value.cooked.as_deref().unwrap_or("");
        let raw = q.value.raw.as_str();
        let cr = cx.alloc_scratch();
        let ci = cx.intern_string_constant(cooked);
        cx.emit(
            Op::LoadString,
            vec![Operand::Register(cr), Operand::ConstIndex(ci)],
            span,
        );
        let rr = cx.alloc_scratch();
        let ri = cx.intern_string_constant(raw);
        cx.emit(
            Op::LoadString,
            vec![Operand::Register(rr), Operand::ConstIndex(ri)],
            span,
        );
        cooked_regs.push(cr);
        raw_regs.push(rr);
    }

    // Materialise the cooked array.
    let strings_reg = cx.alloc_scratch();
    let mut cooked_operands: Vec<Operand> = Vec::with_capacity(2 + cooked_regs.len());
    cooked_operands.push(Operand::Register(strings_reg));
    cooked_operands.push(Operand::ConstIndex(cooked_regs.len() as u32));
    cooked_operands.extend(cooked_regs.iter().copied().map(Operand::Register));
    cx.emit(Op::NewArray, cooked_operands, span);

    // Materialise the raw array.
    let raw_arr_reg = cx.alloc_scratch();
    let mut raw_operands: Vec<Operand> = Vec::with_capacity(2 + raw_regs.len());
    raw_operands.push(Operand::Register(raw_arr_reg));
    raw_operands.push(Operand::ConstIndex(raw_regs.len() as u32));
    raw_operands.extend(raw_regs.iter().copied().map(Operand::Register));
    cx.emit(Op::NewArray, raw_operands, span);

    // Attach `strings.raw = raw_arr`.
    cx.emit_store_property(strings_reg, "raw", raw_arr_reg, span);

    // Evaluate interpolations.
    let mut arg_regs: Vec<u16> = Vec::with_capacity(1 + t.quasi.expressions.len());
    arg_regs.push(strings_reg);
    for expr in t.quasi.expressions.iter() {
        arg_regs.push(compile_expr(cx, expr, span)?);
    }

    // Emit `tag(strings, ...exprs)`.
    let dst = cx.alloc_scratch();
    let mut call_operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    call_operands.push(Operand::Register(dst));
    call_operands.push(Operand::Register(tag_reg));
    call_operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    call_operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::Call, call_operands, span);
    Ok(dst)
}

/// Inline §22.1.2.4 `String.raw` for the tagged-template call shape.
/// Walks raw quasi text + interpolations, concatenating each.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-string.raw>
fn compile_string_raw_template(
    cx: &mut Compiler,
    quasi: &oxc_ast::ast::TemplateLiteral<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    // Seed accumulator with the first raw quasi.
    let mut acc = {
        let raw = quasi.quasis[0].value.raw.as_str();
        let dst = cx.alloc_scratch();
        let const_idx = cx.intern_string_constant(raw);
        cx.emit(
            Op::LoadString,
            vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
            span,
        );
        dst
    };
    for (i, expr) in quasi.expressions.iter().enumerate() {
        let expr_reg = compile_expr(cx, expr, span)?;
        let lhs_in = emit_to_primitive(cx, acc, "default", span);
        let rhs_in = emit_to_primitive(cx, expr_reg, "default", span);
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::Add,
            vec![
                Operand::Register(dst),
                Operand::Register(lhs_in),
                Operand::Register(rhs_in),
            ],
            span,
        );
        acc = dst;
        let raw = quasi.quasis[i + 1].value.raw.as_str();
        if !raw.is_empty() {
            let qr = cx.alloc_scratch();
            let const_idx = cx.intern_string_constant(raw);
            cx.emit(
                Op::LoadString,
                vec![Operand::Register(qr), Operand::ConstIndex(const_idx)],
                span,
            );
            let lhs_in = emit_to_primitive(cx, acc, "default", span);
            let rhs_in = emit_to_primitive(cx, qr, "default", span);
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::Add,
                vec![
                    Operand::Register(dst),
                    Operand::Register(lhs_in),
                    Operand::Register(rhs_in),
                ],
                span,
            );
            acc = dst;
        }
    }
    Ok(acc)
}

/// Lower an optional chain `a?.b?.c?.()` per §13.3.9.
///
/// # Algorithm
/// 1. Walk to the chain root, collecting each step (member access /
///    call) and its `optional` flag in source order.
/// 2. Compile the root, then apply each step:
///    - Evaluate the receiver into a scratch register.
///    - If the step is optional, emit `JumpIfNullish receiver →
///      exit` to short-circuit when the receiver is `null` or
///      `undefined`. The exit target writes `undefined` into the
///      result register.
///    - Otherwise, perform the property load / call as usual.
/// 3. After the final step writes its value, emit an unconditional
///    jump past the exit handler so the chain's success result lands
///    directly in the output register.
///
/// Foundation simplifications:
/// - Optional `super` chains (`super?.foo`) are illegal per §15.6.4
///   and not exercised here.
/// - `delete a?.b` follows the no-op rule §13.3.9.5; foundation
///   relies on the chain producing `undefined` and the regular
///   `delete` path handling the rest.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-optional-chains>
/// - <https://tc39.es/ecma262/#sec-optional-chaining-evaluation>
fn compile_chain_expression(
    cx: &mut Compiler,
    chain: &oxc_ast::ast::ChainExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (chain.span.start, chain.span.end);
    let result = cx.alloc_scratch();
    let exits = compile_chain_into(cx, &chain.expression, result)?;
    // Success path falls through here — jump past the undefined
    // writer so the chain's value lives in `result`.
    let join = cx.emit_branch_placeholder(Op::Jump, None, span);
    // Land every optional-step short-circuit at the undefined writer.
    for pc in exits {
        cx.patch_branch_to_here(pc);
    }
    cx.emit(Op::LoadUndefined, vec![Operand::Register(result)], span);
    cx.patch_branch_to_here(join);
    Ok(result)
}

/// Recursive helper: compile one chain element, writing the success
/// result into `result_reg`. Returns the list of short-circuit
/// jump PCs that should land at the chain's `undefined` writer.
fn compile_chain_into(
    cx: &mut Compiler,
    elem: &oxc_ast::ast::ChainElement<'_>,
    result_reg: u16,
) -> Result<Vec<u32>, CompileError> {
    use oxc_ast::ast::ChainElement;
    match elem {
        ChainElement::CallExpression(call) => {
            let span = (call.span.start, call.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let callee_reg = compile_chain_callee(cx, &call.callee, &mut exits)?;
            if call.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(callee_reg), span);
                exits.push(pc);
            }
            // Compile call arguments.
            let mut arg_regs: Vec<u16> = Vec::with_capacity(call.arguments.len());
            for arg in call.arguments.iter() {
                if let Some(expr) = arg.as_expression() {
                    arg_regs.push(compile_expr(cx, expr, span)?);
                } else {
                    return Err(CompileError::Unsupported {
                        node: "ChainExpression: spread argument".to_string(),
                        span,
                    });
                }
            }
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(result_reg));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::Call, operands, span);
            Ok(exits)
        }
        ChainElement::StaticMemberExpression(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            Ok(exits)
        }
        ChainElement::ComputedMemberExpression(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            let key_reg = compile_expr(cx, &m.expression, span)?;
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            Ok(exits)
        }
        other => Err(CompileError::Unsupported {
            node: format!("ChainElement ({:?})", std::mem::discriminant(other)),
            span: (0, 0),
        }),
    }
}

/// Compile a chain object — either another chain step (recurse) or a
/// regular expression. Threads short-circuit jump PCs upward.
fn compile_chain_object(
    cx: &mut Compiler,
    expr: &oxc_ast::ast::Expression<'_>,
    exits: &mut Vec<u32>,
) -> Result<u16, CompileError> {
    if let Some(elem) = expression_as_chain_element(expr) {
        let result_reg = cx.alloc_scratch();
        let inner = compile_chain_into_chain_object(cx, elem, result_reg)?;
        exits.extend(inner);
        return Ok(result_reg);
    }
    let span = expression_span(expr);
    compile_expr(cx, expr, span)
}

/// Same as [`compile_chain_object`] but accepts a callee position
/// (the callee of `a?.b()`'s call step).
fn compile_chain_callee(
    cx: &mut Compiler,
    expr: &oxc_ast::ast::Expression<'_>,
    exits: &mut Vec<u32>,
) -> Result<u16, CompileError> {
    if let Some(elem) = expression_as_chain_element(expr) {
        let result_reg = cx.alloc_scratch();
        let inner = compile_chain_into_chain_object(cx, elem, result_reg)?;
        exits.extend(inner);
        return Ok(result_reg);
    }
    let span = expression_span(expr);
    compile_expr(cx, expr, span)
}

/// Internal — same as [`compile_chain_into`] but borrows the element
/// reference rather than cloning, since OXC doesn't expose a free
/// conversion. We inline the dispatch here.
fn compile_chain_into_chain_object(
    cx: &mut Compiler,
    elem: ChainObjectRef<'_>,
    result_reg: u16,
) -> Result<Vec<u32>, CompileError> {
    match elem {
        ChainObjectRef::Static(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            Ok(exits)
        }
        ChainObjectRef::Computed(m) => {
            let span = (m.span.start, m.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let obj_reg = compile_chain_object(cx, &m.object, &mut exits)?;
            if m.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(obj_reg), span);
                exits.push(pc);
            }
            let key_reg = compile_expr(cx, &m.expression, span)?;
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(result_reg),
                    Operand::Register(obj_reg),
                    Operand::Register(key_reg),
                ],
                span,
            );
            Ok(exits)
        }
        ChainObjectRef::Call(c) => {
            let span = (c.span.start, c.span.end);
            let mut exits: Vec<u32> = Vec::new();
            let callee_reg = compile_chain_callee(cx, &c.callee, &mut exits)?;
            if c.optional {
                let pc = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(callee_reg), span);
                exits.push(pc);
            }
            let mut arg_regs: Vec<u16> = Vec::with_capacity(c.arguments.len());
            for arg in c.arguments.iter() {
                if let Some(expr) = arg.as_expression() {
                    arg_regs.push(compile_expr(cx, expr, span)?);
                } else {
                    return Err(CompileError::Unsupported {
                        node: "ChainExpression: spread argument".to_string(),
                        span,
                    });
                }
            }
            let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
            operands.push(Operand::Register(result_reg));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::ConstIndex(arg_regs.len() as u32));
            operands.extend(arg_regs.into_iter().map(Operand::Register));
            cx.emit(Op::Call, operands, span);
            Ok(exits)
        }
    }
}

/// Borrowed handle for an inner chain step — avoids cloning OXC's
/// arena-allocated nodes.
enum ChainObjectRef<'a> {
    Static(&'a oxc_ast::ast::StaticMemberExpression<'a>),
    Computed(&'a oxc_ast::ast::ComputedMemberExpression<'a>),
    Call(&'a oxc_ast::ast::CallExpression<'a>),
}

fn expression_as_chain_element<'a>(
    expr: &'a oxc_ast::ast::Expression<'a>,
) -> Option<ChainObjectRef<'a>> {
    match expr {
        Expression::StaticMemberExpression(m) => Some(ChainObjectRef::Static(m)),
        Expression::ComputedMemberExpression(m) => Some(ChainObjectRef::Computed(m)),
        Expression::CallExpression(c) => Some(ChainObjectRef::Call(c)),
        _ => None,
    }
}

fn expression_span(expr: &oxc_ast::ast::Expression<'_>) -> (u32, u32) {
    use oxc_span::GetSpan;
    let s = expr.span();
    (s.start, s.end)
}

/// Emit `Op::ToPrimitive(hint)` reading from `src_reg` and writing
/// into a fresh scratch register; return the scratch register.
///
/// Used by the `+` lowering path to satisfy §13.15.4
/// `ApplyStringOrNumericBinaryOperator` step 1: both operands must
/// pass through `ToPrimitive(default)` before the runtime decides
/// between string concat and numeric add. The runtime fast-path
/// short-circuits on already-primitive values, so the extra
/// instruction is cheap.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-toprimitive>
fn emit_to_primitive(cx: &mut Compiler, src_reg: u16, hint: &str, span: (u32, u32)) -> u16 {
    let dst = cx.alloc_scratch();
    let hint_idx = cx.intern_string_constant(hint);
    cx.emit(
        Op::ToPrimitive,
        vec![
            Operand::Register(dst),
            Operand::Register(src_reg),
            Operand::ConstIndex(hint_idx),
        ],
        span,
    );
    dst
}

fn compile_call_args(
    cx: &mut Compiler,
    args: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<Vec<u16>, CompileError> {
    let mut regs: Vec<u16> = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            oxc_ast::ast::Argument::SpreadElement(s) => {
                return Err(CompileError::Unsupported {
                    node: "Argument::SpreadElement".to_string(),
                    span: (s.span.start, s.span.end),
                });
            }
            other => {
                let expr = other.to_expression();
                regs.push(compile_expr(cx, expr, span)?);
            }
        }
    }
    Ok(regs)
}

/// Emit the bytecode that builds a fresh `Array` register holding
/// the call arguments fanned out from spreads. Returns the
/// register that holds the resulting array. Used by the spread-in-
/// call path; pure regular argument lists keep the dedicated
/// fast path in [`compile_call_args`] / [`Op::Call`].
fn compile_spread_call_args(
    cx: &mut Compiler,
    args: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::NewArray,
        vec![Operand::Register(dst), Operand::ConstIndex(0)],
        span,
    );
    for arg in args {
        match arg {
            oxc_ast::ast::Argument::SpreadElement(s) => {
                let inner_span = (s.span.start, s.span.end);
                emit_spread_into_array(cx, dst, &s.argument, inner_span)?;
            }
            other => {
                let r = compile_expr(cx, other.to_expression(), span)?;
                cx.emit(
                    Op::ArrayPush,
                    vec![Operand::Register(dst), Operand::Register(r)],
                    span,
                );
            }
        }
    }
    Ok(dst)
}

/// Append every element of `iterable` (already materialised as an
/// expression) into the array in `dst_reg`. Lowered as a tight
/// `IteratorNext` loop over a fresh iterator. Shared between the
/// array-literal spread path and the call-argument spread path.
fn emit_spread_into_array(
    cx: &mut Compiler,
    dst_reg: u16,
    iterable: &Expression<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let iterable_reg = compile_expr(cx, iterable, span)?;
    let iter_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetIterator,
        vec![Operand::Register(iter_reg), Operand::Register(iterable_reg)],
        span,
    );
    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();
    let loop_top = cx.next_pc;
    cx.emit(
        Op::IteratorNext,
        vec![
            Operand::Register(value_reg),
            Operand::Register(done_reg),
            Operand::Register(iter_reg),
        ],
        span,
    );
    let exit = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
    cx.emit(
        Op::ArrayPush,
        vec![Operand::Register(dst_reg), Operand::Register(value_reg)],
        span,
    );
    let back = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch(back, loop_top);
    cx.patch_branch_to_here(exit);
    Ok(())
}

fn expr_kind_name(expr: &Expression<'_>) -> &'static str {
    use Expression::*;
    match expr {
        Identifier(_) => "Identifier",
        StringLiteral(_) => "StringLiteral",
        NumericLiteral(_) => "NumericLiteral",
        BooleanLiteral(_) => "BooleanLiteral",
        NullLiteral(_) => "NullLiteral",
        TemplateLiteral(_) => "TemplateLiteral",
        BinaryExpression(_) => "BinaryExpression",
        StaticMemberExpression(_) => "StaticMemberExpression",
        CallExpression(_) => "CallExpression",
        FunctionExpression(_) => "FunctionExpression",
        ArrayExpression(_) => "ArrayExpression",
        ObjectExpression(_) => "ObjectExpression",
        ParenthesizedExpression(_) => "ParenthesizedExpression",
        _ => "Expression",
    }
}

fn expr_span(expr: &Expression<'_>) -> (u32, u32) {
    use oxc_span::GetSpan;
    let s = expr.span();
    (s.start, s.end)
}

/// Strip TypeScript-only expression wrappers and parentheses,
/// returning the underlying runtime expression.
///
/// Recognises `TSAsExpression`, `TSSatisfiesExpression`,
/// `TSNonNullExpression`, `TSTypeAssertion`, and
/// `TSInstantiationExpression`. Also unwraps
/// `ParenthesizedExpression` so `(undefined as any)` and
/// `(((x as A) satisfies B)!)` collapse to their leaf expressions.
/// Recursive.
#[must_use]
pub fn unwrap_ts_expr<'a, 'b>(expr: &'a Expression<'b>) -> &'a Expression<'b> {
    match expr {
        Expression::TSAsExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSNonNullExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSTypeAssertion(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSInstantiationExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::ParenthesizedExpression(inner) => unwrap_ts_expr(&inner.expression),
        other => other,
    }
}

/// `true` for top-level TS statements that the frontend policy marks as
/// "erased" — they produce no bytecode and are not errors.
fn is_erased_ts_statement(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::TSTypeAliasDeclaration(_)
        | Statement::TSInterfaceDeclaration(_)
        | Statement::TSImportEqualsDeclaration(_) => true,

        // `declare function f();` and friends.
        Statement::FunctionDeclaration(f) if f.declare => true,
        Statement::ClassDeclaration(c) if c.declare => true,
        Statement::VariableDeclaration(v) if v.declare => true,

        // `import type { X } from "y"` / `import { type X } from "y"`
        // — when the whole import is type-only the declaration is
        // erased; otherwise this slice does not yet support imports.
        Statement::ImportDeclaration(d) if d.import_kind.is_type() => true,

        // `export type { ... }` / `export type X = ...`
        Statement::ExportNamedDeclaration(d) if d.export_kind.is_type() => true,
        Statement::ExportAllDeclaration(d) if d.export_kind.is_type() => true,

        // `declare module "..." { ... }` and `declare namespace N { ... }`.
        Statement::TSModuleDeclaration(m) if m.declare => true,

        _ => false,
    }
}

/// `Some((node, span))` for top-level TS statements that the frontend policy
/// marks as "diagnosed" — produce a structured `TS_UNSUPPORTED`.
fn rejected_ts_statement(stmt: &Statement<'_>) -> Option<(&'static str, (u32, u32))> {
    use oxc_span::GetSpan;
    match stmt {
        Statement::TSEnumDeclaration(d) => Some(("TSEnumDeclaration", (d.span.start, d.span.end))),
        // Non-`declare` namespace with a runtime body.
        Statement::TSModuleDeclaration(d) if !d.declare => {
            Some(("TSModuleDeclaration", (d.span.start, d.span.end)))
        }
        Statement::ClassDeclaration(c) if !c.decorators.is_empty() => {
            let s = c.decorators[0].span();
            Some(("Decorator", (s.start, s.end)))
        }
        _ => None,
    }
}

fn stmt_kind_name(stmt: &Statement<'_>) -> &'static str {
    match stmt {
        Statement::EmptyStatement(_) => "EmptyStatement",
        Statement::ExpressionStatement(_) => "ExpressionStatement",
        Statement::VariableDeclaration(_) => "VariableDeclaration",
        Statement::FunctionDeclaration(_) => "FunctionDeclaration",
        Statement::ClassDeclaration(_) => "ClassDeclaration",
        Statement::IfStatement(_) => "IfStatement",
        Statement::ForStatement(_) => "ForStatement",
        Statement::ForOfStatement(_) => "ForOfStatement",
        Statement::WhileStatement(_) => "WhileStatement",
        Statement::DoWhileStatement(_) => "DoWhileStatement",
        Statement::ReturnStatement(_) => "ReturnStatement",
        Statement::ThrowStatement(_) => "ThrowStatement",
        Statement::TryStatement(_) => "TryStatement",
        Statement::BlockStatement(_) => "BlockStatement",
        Statement::TSEnumDeclaration(_) => "TSEnumDeclaration",
        Statement::TSInterfaceDeclaration(_) => "TSInterfaceDeclaration",
        Statement::TSTypeAliasDeclaration(_) => "TSTypeAliasDeclaration",
        Statement::TSModuleDeclaration(_) => "TSModuleDeclaration",
        Statement::ImportDeclaration(_) => "ImportDeclaration",
        Statement::ExportNamedDeclaration(_) => "ExportNamedDeclaration",
        Statement::ExportDefaultDeclaration(_) => "ExportDefaultDeclaration",
        Statement::ExportAllDeclaration(_) => "ExportAllDeclaration",
        _ => "Statement",
    }
}

fn stmt_span(stmt: &Statement<'_>) -> (u32, u32) {
    use oxc_span::GetSpan;
    let s = stmt.span();
    (s.start, s.end)
}

/// Concrete compiler errors.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CompileError {
    /// Parsing failed in `otter-syntax`.
    #[error("syntax: {}", .messages.join("; "))]
    Syntax {
        /// One message per OXC parser diagnostic.
        messages: Vec<String>,
        /// Structured parser diagnostics with byte ranges and help text.
        diagnostics: Vec<SyntaxDiagnostic>,
    },
    /// The AST node is recognized but not supported by this slice.
    #[error("unsupported {node} at offset {}-{}", .span.0, .span.1)]
    Unsupported {
        /// AST node kind name.
        node: String,
        /// Source span of the offending node.
        span: (u32, u32),
    },
    /// A TypeScript construct is intentionally rejected by the
    /// frontend policy (e.g., `enum`, runtime `namespace`,
    /// decorators).
    #[error("typescript construct {node} is not supported in foundation")]
    TypeScriptUnsupported {
        /// AST node kind name.
        node: String,
        /// Source span of the offending node.
        span: (u32, u32),
    },
}

impl From<SyntaxError> for CompileError {
    fn from(error: SyntaxError) -> Self {
        Self::Syntax {
            messages: error.messages,
            diagnostics: error.diagnostics,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_syntax::parse;

    fn host_info(specifiers: &[(&str, &str)]) -> ModuleHostInfo {
        ModuleHostInfo {
            module_url: "file:///test/main.ts".to_string(),
            resolved_imports: specifiers
                .iter()
                .map(|(s, t)| (s.to_string(), t.to_string()))
                .collect(),
        }
    }

    #[test]
    fn module_fragment_marks_module_init() {
        let parsed = parse("export let x = 7;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile_module_fragment(&parsed, &host_info(&[])).unwrap();
        let init = &module.functions[0];
        assert!(init.is_module);
        assert_eq!(init.name, "<module-init>");
        assert_eq!(init.module_url, "file:///test/main.ts");
        assert_eq!(init.param_count, 2);
        assert_eq!(module.module, "file:///test/main.ts");
        // Two own-upvalues for module_env + import_meta.
        assert!(init.own_upvalue_count >= 2);
    }

    #[test]
    fn module_export_mirrors_assignment() {
        let parsed = parse(
            "export let counter = 0; counter = counter + 1;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile_module_fragment(&parsed, &host_info(&[])).unwrap();
        let init = &module.functions[0];
        // Two StoreProperty ops expected: initial declaration
        // mirror + assignment mirror.
        let store_property_count = init
            .code
            .iter()
            .filter(|i| i.op == Op::StoreProperty)
            .count();
        assert!(
            store_property_count >= 2,
            "expected >=2 StoreProperty mirrors, got {store_property_count}"
        );
    }

    #[test]
    fn module_import_lowers_to_load_property_chain() {
        let src = "import { value } from \"./other.ts\"; let y = value;";
        let parsed = parse(src, SyntaxSourceKind::TypeScript).unwrap();
        let host = host_info(&[("./other.ts", "file:///test/other.ts")]);
        let module = compile_module_fragment(&parsed, &host).unwrap();
        let init = &module.functions[0];
        // ImportNamespace at the top of the body.
        assert!(init.code.iter().any(|i| i.op == Op::ImportNamespace));
        // LoadProperty for the read of `value`.
        assert!(init.code.iter().any(|i| i.op == Op::LoadProperty));
        // module_resolutions populated from host info.
        assert_eq!(module.module_resolutions.len(), 1);
        assert_eq!(module.module_resolutions[0].specifier, "./other.ts");
        assert_eq!(module.module_resolutions[0].target, "file:///test/other.ts");
    }

    #[test]
    fn import_outside_module_mode_is_rejected() {
        let parsed = parse(
            "import { a } from \"./x.ts\";",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        match err {
            CompileError::Unsupported { node, .. } => {
                assert!(node.contains("ImportDeclaration"), "got {node}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn dynamic_import_with_non_literal_argument_compiles() {
        // Non-literal specifiers now lower through
        // `Op::ImportNamespaceDynamic`. The runtime resolves the
        // string against the active module's resolution table.
        let src = "let s = \"./x.ts\"; import(s);";
        let parsed = parse(src, SyntaxSourceKind::TypeScript).unwrap();
        let module = compile_module_fragment(&parsed, &host_info(&[])).unwrap();
        let init = &module.functions[0];
        let dyn_count = init
            .code
            .iter()
            .filter(|i| matches!(i.op, Op::ImportNamespaceDynamic))
            .count();
        assert_eq!(dyn_count, 1, "expected one IMPORT_NAMESPACE_DYNAMIC");
    }

    #[test]
    fn import_meta_lowers_to_load_upvalue() {
        let src = "let u = import.meta.url;";
        let parsed = parse(src, SyntaxSourceKind::TypeScript).unwrap();
        let module = compile_module_fragment(&parsed, &host_info(&[])).unwrap();
        let init = &module.functions[0];
        // The body should LoadUpvalue then LoadProperty for .url.
        let load_upvalue_count = init.code.iter().filter(|i| i.op == Op::LoadUpvalue).count();
        assert!(
            load_upvalue_count >= 1,
            "expected at least one LoadUpvalue (import.meta), got {load_upvalue_count}"
        );
        assert!(init.code.iter().any(|i| i.op == Op::LoadProperty));
    }

    #[test]
    fn bigint_literal_emits_load_bigint() {
        let parsed = parse("123n;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::LoadBigInt));
        let interned = module
            .constants
            .iter()
            .any(|c| matches!(c, otter_bytecode::Constant::BigInt { decimal } if decimal == "123"));
        assert!(interned, "BigInt constant should round-trip the decimal");
    }

    #[test]
    fn bitwise_binary_ops_lower_directly() {
        let parsed = parse(
            "5 & 3; 5 | 3; 5 ^ 3; 1 << 3; -1 >> 1; -1 >>> 0;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        for op in [
            Op::BitwiseAnd,
            Op::BitwiseOr,
            Op::BitwiseXor,
            Op::Shl,
            Op::Shr,
            Op::Ushr,
        ] {
            assert!(
                main.code.iter().any(|i| i.op == op),
                "missing {op:?} in {:?}",
                main.code
            );
        }
    }

    #[test]
    fn pow_operator_emits_pow() {
        let parsed = parse("2 ** 10;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::Pow));
    }

    #[test]
    fn compound_assign_load_modify_store() {
        let parsed = parse("let n = 4; n &= 1; n **= 2;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::BitwiseAnd));
        assert!(main.code.iter().any(|i| i.op == Op::Pow));
    }

    #[test]
    fn math_namespace_lowers_to_dedicated_ops() {
        let parsed = parse(
            "Math.PI; Math.abs(-1); Math.max(1, 2, 3);",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::MathLoad));
        let calls = main.code.iter().filter(|i| i.op == Op::MathCall).count();
        assert_eq!(calls, 2);
    }

    #[test]
    fn rest_param_marks_function_and_emits_collect_rest() {
        let parsed = parse(
            "function f(...rest) { return rest.length; }",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let f = &module.functions[1];
        assert!(f.has_rest, "rest flag should be set");
        assert_eq!(f.param_count, 0);
        assert!(f.code.iter().any(|i| i.op == Op::CollectRest));
    }

    #[test]
    fn default_param_emits_undefined_check() {
        let parsed = parse(
            "function f(a, b = 5) { return a + b; }",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let f = &module.functions[1];
        // Default lowering emits LoadUndefined + Equal + JumpIfFalse
        // before the body's normal store. Their presence is a
        // sufficient witness that the default path was taken.
        assert!(f.code.iter().any(|i| i.op == Op::LoadUndefined));
        assert!(f.code.iter().any(|i| i.op == Op::Equal));
        assert!(f.code.iter().any(|i| i.op == Op::JumpIfFalse));
    }

    #[test]
    fn array_destructure_uses_iterator_protocol() {
        let parsed = parse(
            "const [a, b, ...rest] = [1, 2, 3, 4];",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::GetIterator));
        assert!(main.code.iter().any(|i| i.op == Op::IteratorNext));
        // Rest tail copies through ArrayPush.
        assert!(main.code.iter().any(|i| i.op == Op::ArrayPush));
    }

    #[test]
    fn object_destructure_loads_each_key() {
        let parsed = parse(
            "function f({ x, y = 9 }) { return x + y; }",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let f = &module.functions[1];
        // Two property loads (one per declared key), with the
        // default applied to `y`.
        let loads = f.code.iter().filter(|i| i.op == Op::LoadProperty).count();
        assert!(
            loads >= 2,
            "expected at least 2 LoadProperty ops, got {loads}: {:?}",
            f.code
        );
    }

    #[test]
    fn for_of_emits_iterator_dispatch() {
        let parsed = parse("for (let n of [1, 2]) { n; }", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::GetIterator));
        assert!(main.code.iter().any(|i| i.op == Op::IteratorNext));
    }

    #[test]
    fn array_literal_spread_emits_array_push_loop() {
        let parsed = parse(
            "const inner = [1, 2]; [0, ...inner, 3];",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::GetIterator));
        assert!(main.code.iter().any(|i| i.op == Op::ArrayPush));
    }

    #[test]
    fn spread_call_emits_call_spread() {
        let parsed = parse(
            "function f(a, b) { return a + b; } f(...[1, 2]);",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::CallSpread));
    }

    #[test]
    fn throw_statement_emits_throw_op() {
        let parsed = parse("throw new Error(\"x\");", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::NewError));
        assert!(main.code.iter().any(|i| i.op == Op::Throw));
    }

    #[test]
    fn try_catch_emits_enter_and_leave() {
        let parsed = parse(
            "try { throw new Error(\"x\"); } catch (e) { e; }",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::EnterTry));
        assert!(main.code.iter().any(|i| i.op == Op::LeaveTry));
        // No finally → no EndFinally.
        assert!(!main.code.iter().any(|i| i.op == Op::EndFinally));
    }

    #[test]
    fn try_finally_emits_end_finally() {
        let parsed = parse("try { 1; } finally { 2; }", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::EnterTry));
        assert!(main.code.iter().any(|i| i.op == Op::EndFinally));
    }

    #[test]
    fn try_catch_finally_emits_two_enter_try_blocks() {
        let parsed = parse(
            "try { throw new Error(\"x\"); } catch (e) { e; } finally { 1; }",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        let enters = main.code.iter().filter(|i| i.op == Op::EnterTry).count();
        assert_eq!(
            enters, 2,
            "try/catch/finally should emit two EnterTry blocks: {:?}",
            main.code
        );
        assert!(main.code.iter().any(|i| i.op == Op::EndFinally));
    }

    #[test]
    fn class_declaration_emits_make_class_and_new() {
        let parsed = parse(
            "class Foo { speak() { return 1; } } new Foo();",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::MakeClass));
        assert!(main.code.iter().any(|i| i.op == Op::New));
    }

    #[test]
    fn this_expression_emits_load_this() {
        let parsed = parse("this;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::LoadThis),
            "expected LoadThis in {:?}",
            main.code
        );
    }

    #[test]
    fn method_call_emits_call_method_value() {
        let parsed = parse(
            "const o = { v: 1 }; o.toString();",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallMethodValue),
            "expected CallMethodValue: {:?}",
            main.code
        );
    }

    #[test]
    fn fn_call_lowers_to_call_with_this() {
        let parsed = parse(
            "function f() { return this; } f.call({});",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallWithThis),
            "expected CallWithThis: {:?}",
            main.code
        );
    }

    #[test]
    fn fn_apply_with_array_literal_unpacks() {
        let parsed = parse(
            "function f(a, b) { return a + b; } f.apply({}, [1, 2]);",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallWithThis),
            "apply with literal array should lower to CallWithThis: {:?}",
            main.code
        );
    }

    #[test]
    fn fn_apply_with_dynamic_args_rejected() {
        let parsed = parse(
            "function f() {} const args = [1]; f.apply({}, args);",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        assert!(
            matches!(err, CompileError::Unsupported { ref node, .. } if node.contains("apply")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn fn_bind_emits_bind_function() {
        let parsed = parse(
            "function f() {} f.bind({}, 1, 2);",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::BindFunction),
            "expected BindFunction: {:?}",
            main.code
        );
    }

    #[test]
    fn arrow_record_marked_arrow_and_emits_make_closure() {
        let parsed = parse("const f = () => 1; f();", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        // Arrows always go through MakeClosure (even with zero
        // captures) so the runtime can snapshot enclosing `this`.
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::MakeClosure),
            "arrow should emit MakeClosure: {:?}",
            main.code
        );
        let arrow_fn = module
            .functions
            .iter()
            .find(|f| f.is_arrow)
            .expect("arrow function record");
        assert_eq!(arrow_fn.name, "<arrow>");
    }

    #[test]
    fn closure_emits_make_closure_with_capture() {
        let parsed = parse(
            "function makeCounter() { let n = 0; return function() { n = n + 1; return n; }; }\nmakeCounter();",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        // The inner function captures `n` from `makeCounter`, so the
        // outer body emits `MakeClosure` instead of `MakeFunction`.
        let outer = &module.functions[1];
        let has_make_closure = outer.code.iter().any(|i| i.op == Op::MakeClosure);
        assert!(
            has_make_closure,
            "outer function should emit MakeClosure for capturing inner: {:?}",
            outer.code
        );
        // The inner function reads / writes `n` through upvalue ops.
        let inner = &module.functions[2];
        assert!(
            inner.code.iter().any(|i| i.op == Op::LoadUpvalue),
            "inner should LoadUpvalue: {:?}",
            inner.code
        );
        assert!(
            inner.code.iter().any(|i| i.op == Op::StoreUpvalue),
            "inner should StoreUpvalue: {:?}",
            inner.code
        );
    }

    #[test]
    fn empty_script_compiles() {
        let parsed = parse("", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert_eq!(main.code.len(), 2);
        assert_eq!(main.code[0].op, Op::LoadUndefined);
        assert_eq!(main.code[1].op, Op::Return);
    }

    #[test]
    fn undefined_literal_compiles() {
        let parsed = parse("undefined;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert_eq!(main.code.len(), 2);
        assert_eq!(main.code[0].op, Op::LoadUndefined);
        assert_eq!(main.code[1].op, Op::Return);
    }

    #[test]
    fn unsupported_statement_rejects() {
        // `with` is not in the foundation subset and never will be —
        // expect the Unsupported diagnostic with a descriptive node
        // name. (`try`/`catch` shipped in task 24 so it's no longer
        // a useful exemplar here.)
        let parsed = parse("with (o) { x; }", SyntaxSourceKind::TypeScript).unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        assert!(matches!(err, CompileError::Unsupported { .. }));
    }

    #[test]
    fn type_alias_is_erased() {
        let parsed = parse(
            "type Foo = number; undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        // LoadUndefined for the body + Return.
        let main = module.main();
        assert_eq!(main.code.len(), 2);
    }

    #[test]
    fn interface_is_erased() {
        let parsed = parse(
            "interface I { x: number; } undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn declare_function_is_erased() {
        let parsed = parse(
            "declare function foo(): void; undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn import_type_is_erased() {
        let parsed = parse(
            "import type { Foo } from \"./foo\"; undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn as_expression_unwraps_to_undefined() {
        let parsed = parse("(undefined as any);", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        // `(undefined as any)` is statement-level; LoadUndefined + Return.
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn satisfies_expression_unwraps_to_undefined() {
        let parsed = parse(
            "(undefined satisfies unknown);",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn non_null_unwraps_to_undefined() {
        let parsed = parse("undefined!;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn enum_is_rejected_with_ts_unsupported() {
        let parsed = parse("enum E { A }", SyntaxSourceKind::TypeScript).unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        match err {
            CompileError::TypeScriptUnsupported { node, .. } => {
                assert_eq!(node, "TSEnumDeclaration");
            }
            other => panic!("expected TypeScriptUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn namespace_with_runtime_body_is_rejected() {
        let parsed = parse(
            "namespace N { export const x = 1; }",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        assert!(matches!(err, CompileError::TypeScriptUnsupported { .. }));
    }

    #[test]
    fn declared_namespace_is_erased() {
        let parsed = parse(
            "declare namespace N { function f(): void; } undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn string_literal_compiles_to_load_string() {
        // Parenthesize to keep OXC from treating the bare literal
        // as a directive prologue.
        let parsed = parse("(\"abc\");", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert_eq!(main.code.len(), 2);
        assert_eq!(main.code[0].op, Op::LoadString);
        assert_eq!(main.code[1].op, Op::Return);
        assert_eq!(module.constants.len(), 1);
        let Constant::String { utf16 } = &module.constants[0] else {
            panic!("expected String constant");
        };
        assert_eq!(utf16, &vec![b'a' as u16, b'b' as u16, b'c' as u16]);
    }

    #[test]
    fn string_concat_compiles_to_add() {
        let parsed = parse("\"a\" + \"b\";", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::Add));
    }

    #[test]
    fn strict_equals_compiles_to_eq() {
        let parsed = parse("\"a\" === \"a\";", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::Equal));
    }

    #[test]
    fn numeric_literal_smi_compiles_to_load_int32() {
        let parsed = parse("(42);", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadInt32));
    }

    #[test]
    fn arithmetic_lowers_to_numeric_ops() {
        let parsed = parse("1 + 2 * 3 - 4 / 5;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let ops: Vec<Op> = module.main().code.iter().map(|i| i.op).collect();
        assert!(ops.contains(&Op::Add));
        assert!(ops.contains(&Op::Sub));
        assert!(ops.contains(&Op::Mul));
        assert!(ops.contains(&Op::Div));
    }

    #[test]
    fn unary_minus_lowers_to_neg() {
        let parsed = parse("-(5);", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::Neg));
    }

    #[test]
    fn boolean_literal_lowers() {
        let parsed = parse("(true);", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadTrue));
    }

    #[test]
    fn dot_length_compiles_to_load_property() {
        // Slice 17 generalised `.length` into the same
        // `LoadProperty` opcode used for object property access;
        // the runtime keeps the string-length fast path inside
        // the dispatcher.
        let parsed = parse("\"abc\".length;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadProperty));
    }

    #[test]
    fn template_no_interpolation_compiles_to_load_string() {
        let parsed = parse("`abc`;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadString));
    }

    #[test]
    fn duplicate_string_literals_share_constant() {
        let parsed = parse("(\"abc\"); (\"abc\");", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.constants.len(), 1);
    }
}
