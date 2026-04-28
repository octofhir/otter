//! AST → bytecode lowering with full foundation TS erasure.
//!
//! The compiler walks the OXC AST produced by `otter-syntax` and
//! emits an [`otter_bytecode::BytecodeModule`]. After task 08 the
//! frontend handles the **complete** foundation TypeScript subset
//! per [ADR-0002 §4](
//!     ../../../docs/new-engine/adr/0002-oxc-frontend.md
//!   ):
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
//! - [`docs/new-engine/adr/0002-oxc-frontend.md`](
//!     ../../../docs/new-engine/adr/0002-oxc-frontend.md
//!   )

mod capture;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use otter_bytecode::{
    BytecodeModule, Constant, Function, Instruction, Op, Operand, SourceKind as BytecodeSourceKind,
    SpanEntry,
};
use otter_syntax::{Parsed, SourceKind as SyntaxSourceKind};
use oxc_ast::ast::{
    AssignmentOperator, AssignmentTarget, BinaryOperator, Expression, LogicalOperator, Statement,
    UnaryOperator,
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
    let program = parsed.program().map_err(|e| CompileError::Syntax {
        messages: e.messages,
    })?;

    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    // Reserve slot 0 for `<main>` so nested function compilation
    // can pre-register their ids deterministically (slice 13 only
    // needs the immediate id, but the slot reservation keeps the
    // table densely populated).
    module.borrow_mut().functions.push(Function {
        id: 0,
        name: "<main>".to_string(),
        span: (program.span.start, program.span.end),
        ..Default::default()
    });
    let mut top = FunctionContext::new(Rc::clone(&module));
    top.captured_names = capture::analyze_module(&program.body);
    let mut cx = Compiler::new(top);
    cx.enter_scope();
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

    let kind = match parsed.kind {
        SyntaxSourceKind::JavaScript => BytecodeSourceKind::JavaScript,
        SyntaxSourceKind::TypeScript => BytecodeSourceKind::TypeScript,
    };

    let ModuleBuilder {
        functions,
        constants,
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
    let program = parsed.program().map_err(|e| CompileError::Syntax {
        messages: e.messages,
    })?;

    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    module.borrow_mut().functions.push(Function {
        id: 0,
        name: "<module-init>".to_string(),
        span: (program.span.start, program.span.end),
        is_module: true,
        module_url: host.module_url.clone(),
        param_count: 2, // module_env, import_meta
        ..Default::default()
    });

    let mut top = FunctionContext::new(Rc::clone(&module));
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

    let kind = match parsed.kind {
        SyntaxSourceKind::JavaScript => BytecodeSourceKind::JavaScript,
        SyntaxSourceKind::TypeScript => BytecodeSourceKind::TypeScript,
    };

    let ModuleBuilder {
        functions,
        constants,
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
            if is_var {
                return Err(CompileError::Unsupported {
                    node: "export var (foundation rejects var)".to_string(),
                    span,
                });
            }
            for declarator in &v.declarations {
                let dspan = (declarator.span.start, declarator.span.end);
                let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id else {
                    return Err(CompileError::Unsupported {
                        node: "export with destructuring not yet supported".to_string(),
                        span: dspan,
                    });
                };
                let name = id.name.as_str().to_string();
                let storage = cx.declare_binding(&name, is_const, dspan)?;
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
            let (function_id, captures) =
                compile_function(cx, &name, &f.params, &f.body, fspan, f.r#async)?;
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
            let storage = cx.declare_binding(&name, false, cspan)?;
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
fn module_export_name_to_str(name: &oxc_ast::ast::ModuleExportName<'_>) -> String {
    match name {
        oxc_ast::ast::ModuleExportName::IdentifierName(id) => id.name.as_str().to_string(),
        oxc_ast::ast::ModuleExportName::IdentifierReference(id) => id.name.as_str().to_string(),
        oxc_ast::ast::ModuleExportName::StringLiteral(lit) => lit.value.as_str().to_string(),
    }
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

/// One pending loop label so `break` / `continue` can patch their
/// offsets at scope close.
#[derive(Debug, Default)]
struct LoopFrame {
    /// Instruction PCs where `continue` emitted a placeholder
    /// JUMP. Patched to point at the loop's continue target (the
    /// update / test).
    continue_patches: Vec<u32>,
    /// Instruction PCs where `break` emitted a placeholder JUMP.
    /// Patched to point at the instruction after the loop body.
    break_patches: Vec<u32>,
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
    /// Stack of enclosing loops; the innermost is on top.
    loops: Vec<LoopFrame>,
    /// Names of this function's own bindings that some nested
    /// function references — populated by
    /// [`capture::analyze_function`] before code gen starts. Each
    /// such binding is allocated as an
    /// [`UpvalueCell`](otter_vm::UpvalueCell) instead of a register.
    captured_names: HashSet<String>,
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
}

impl Compiler {
    fn new(top: FunctionContext) -> Self {
        Self { stack: vec![top] }
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
            loops: Vec::new(),
            captured_names: HashSet::new(),
            own_upvalue_count: 0,
            parent_captures: Vec::new(),
            captured_uv: HashMap::new(),
            module_state: None,
        }
    }

    /// Check `name` against this function's `captured_names` set
    /// (computed by the pre-pass) and, when present, allocate a
    /// fresh own-upvalue index for it. Returns the assigned index
    /// or `None` if the name is not captured (use a register
    /// instead).
    fn allocate_own_upvalue(&mut self, name: &str) -> Option<u16> {
        if !self.captured_names.contains(name) {
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

    /// Emit `Op::StoreProperty obj_reg, name_const, src_reg`.
    /// Used by the module-mode lowering to mirror writes through
    /// to `module_env` for exported bindings, and by the export
    /// declaration arms.
    fn emit_store_property(&mut self, obj_reg: u16, name: &str, src: u16, span: (u32, u32)) {
        let name_const = self.intern_string_constant(name);
        self.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_const),
                Operand::Register(src),
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
            if is_var {
                return Err(CompileError::Unsupported {
                    node: "VariableDeclaration (var; foundation rejects var)".to_string(),
                    span: (decl.span.start, decl.span.end),
                });
            }
            for declarator in &decl.declarations {
                let span = (declarator.span.start, declarator.span.end);
                // Fast path for the overwhelmingly common
                // `let x = init;` shape so the simple binding
                // doesn't pay an extra register copy.
                if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                    let name = id.name.as_str().to_string();
                    let storage = cx.declare_binding(&name, is_const, span)?;
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
            cx.loops.push(LoopFrame::default());
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
            cx.loops.push(LoopFrame::default());
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
            cx.loops.push(LoopFrame::default());
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
            if s.label.is_some() {
                return Err(CompileError::Unsupported {
                    node: "BreakStatement (labeled)".to_string(),
                    span,
                });
            }
            let loop_idx = cx
                .loops
                .len()
                .checked_sub(1)
                .ok_or(CompileError::Unsupported {
                    node: "BreakStatement outside any loop".to_string(),
                    span,
                })?;
            let pc = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.loops[loop_idx].break_patches.push(pc);
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
            let (function_id, captures) =
                compile_function(cx, &name, &f.params, &f.body, span, f.r#async)?;
            // Bind the name in the current scope to a register
            // holding the function value. Foundation slice doesn't
            // hoist; declarations are evaluated at their lexical
            // position.
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
            let storage = cx.declare_binding(&name, false, span)?;
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
                    let name =
                        f.id.as_ref()
                            .map(|id| id.name.as_str().to_string())
                            .unwrap_or_else(|| "default".to_string());
                    let (function_id, captures) =
                        compile_function(cx, &name, &f.params, &f.body, span, f.r#async)?;
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
            if s.label.is_some() {
                return Err(CompileError::Unsupported {
                    node: "ContinueStatement (labeled)".to_string(),
                    span,
                });
            }
            let loop_idx = cx
                .loops
                .len()
                .checked_sub(1)
                .ok_or(CompileError::Unsupported {
                    node: "ContinueStatement outside any loop".to_string(),
                    span,
                })?;
            let pc = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.loops[loop_idx].continue_patches.push(pc);
            Ok(None)
        }

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
    span: (u32, u32),
) -> Result<(), CompileError> {
    let is_const = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Const);
    let is_var = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
    if is_var {
        return Err(CompileError::Unsupported {
            node: "for-init `var` (foundation rejects var)".to_string(),
            span,
        });
    }
    for declarator in &decl.declarations {
        let span = (declarator.span.start, declarator.span.end);
        let name = match &declarator.id {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) => id.name.as_str().to_string(),
            _ => {
                return Err(CompileError::Unsupported {
                    node: "for-init declarator pattern (non-identifier)".to_string(),
                    span,
                });
            }
        };
        let storage = cx.declare_binding(&name, is_const, span)?;
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
fn compile_function(
    parent: &mut Compiler,
    name: &str,
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &Option<oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody<'_>>>,
    span: (u32, u32),
    is_async: bool,
) -> Result<(u32, Vec<u32>), CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let mut child = FunctionContext::new(Rc::clone(&module));
    if let Some(b) = body {
        child.captured_names = capture::analyze_function(Some(params), b);
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
        )?;
    }
    if let Some(rest) = &params.rest {
        compile_rest_parameter(parent, &rest.rest.argument, span)?;
    }

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

    if let Some(body) = body {
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
        return Err(CompileError::Unsupported {
            node: format!("AssignmentExpression ({:?})", a.operator),
            span,
        });
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
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_idx),
                Operand::Register(new_value),
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
        cx.emit(
            Op::StoreElement,
            vec![
                Operand::Register(arr_reg),
                Operand::Register(idx_reg),
                Operand::Register(new_value),
            ],
            span,
        );
        return Ok(new_value);
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
    let storage = if let Some(info) = cx.lookup_binding(&name) {
        if info.is_const {
            return Err(CompileError::Unsupported {
                node: format!("assignment to const `{name}`"),
                span,
            });
        }
        info.storage
    } else if let Some(idx) = cx.resolve_capture(&name) {
        BindingStorage::Upvalue { idx }
    } else {
        return Err(CompileError::Unsupported {
            node: format!("assignment to undeclared `{name}`"),
            span,
        });
    };
    let value = match compound_op {
        None => compile_expr(cx, &a.right, span)?,
        Some(op) => {
            let current = cx.alloc_scratch();
            cx.emit_load_storage(current, storage, span);
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
    cx.emit_store_storage(value, storage, span);
    cx.mark_initialized(&name);
    cx.emit_module_export_mirror(&name, value, span);
    Ok(value)
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
) -> Result<(), CompileError> {
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
    match pattern {
        oxc_ast::ast::BindingPattern::BindingIdentifier(id) => {
            let name = id.name.as_str().to_string();
            let storage = parent.declare_binding(&name, false, span)?;
            parent.emit_store_storage(src_reg, storage, span);
            parent.mark_initialized(&name);
            Ok(())
        }
        oxc_ast::ast::BindingPattern::AssignmentPattern(asgn) => {
            let asgn_span = (asgn.span.start, asgn.span.end);
            apply_default_into(parent, src_reg, &asgn.right, asgn_span)?;
            destructure_into(parent, src_reg, &asgn.left, span)
        }
        oxc_ast::ast::BindingPattern::ArrayPattern(arr) => {
            destructure_array(parent, src_reg, arr, span)
        }
        oxc_ast::ast::BindingPattern::ObjectPattern(obj) => {
            destructure_object(parent, src_reg, obj, span)
        }
    }
}

fn destructure_array(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::ArrayPattern<'_>,
    span: (u32, u32),
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
        destructure_into(parent, value_reg, inner, span)?;
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
        destructure_into(parent, arr_reg, &rest.argument, span)?;
    }
    Ok(())
}

fn destructure_object(
    parent: &mut Compiler,
    src_reg: u16,
    pattern: &oxc_ast::ast::ObjectPattern<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    if pattern.rest.is_some() {
        return Err(CompileError::Unsupported {
            node: "ObjectPattern: rest element (foundation slice deferred)".to_string(),
            span,
        });
    }
    for prop in &pattern.properties {
        let prop_span = (prop.span.start, prop.span.end);
        if prop.computed {
            return Err(CompileError::Unsupported {
                node: "ObjectPattern: computed key".to_string(),
                span: prop_span,
            });
        }
        let key_str = match &prop.key {
            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
            _ => {
                return Err(CompileError::Unsupported {
                    node: "ObjectPattern: non-string key".to_string(),
                    span: prop_span,
                });
            }
        };
        let key_const = parent.intern_string_constant(&key_str);
        let value_reg = parent.alloc_scratch();
        parent.emit(
            Op::LoadProperty,
            vec![
                Operand::Register(value_reg),
                Operand::Register(src_reg),
                Operand::ConstIndex(key_const),
            ],
            prop_span,
        );
        destructure_into(parent, value_reg, &prop.value, prop_span)?;
    }
    Ok(())
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
    let mut child = FunctionContext::new(Rc::clone(&module));
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
        ..Default::default()
    });

    for (ordinal, param) in arrow.params.items.iter().enumerate() {
        compile_formal_parameter(
            parent,
            ordinal as u16,
            &param.pattern,
            param.initializer.as_deref(),
            span,
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
            Err(CompileError::Unsupported {
                node: format!("unresolved identifier `{}`", id.name),
                span,
            })
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
            let const_idx = cx.intern_string_constant(&lit.value);
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
            // groups). Mirrors the BigIntLiteral approach. The `g`
            // and `y` flags live above the matcher per JS spec, so
            // we strip them before asking `regress` to compile.
            let mut engine_flags = regress::Flags::default();
            for c in flags_str.chars() {
                match c {
                    'g' | 'y' => {}
                    'i' => engine_flags.icase = true,
                    'm' => engine_flags.multiline = true,
                    's' => engine_flags.dot_all = true,
                    'u' => engine_flags.unicode = true,
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
                return Err(CompileError::Unsupported {
                    node: "delete on non-member expression".to_string(),
                    span,
                });
            }
            let inner = compile_expr(cx, &u.argument, span)?;
            let dst = cx.alloc_scratch();
            let op = match u.operator {
                UnaryOperator::UnaryNegation => Op::Neg,
                UnaryOperator::UnaryPlus => Op::ToNumber,
                UnaryOperator::LogicalNot => Op::LogicalNot,
                UnaryOperator::BitwiseNot => Op::BitwiseNot,
                other => {
                    return Err(CompileError::Unsupported {
                        node: format!("UnaryExpression ({other:?})"),
                        span,
                    });
                }
            };
            cx.emit(
                op,
                vec![Operand::Register(dst), Operand::Register(inner)],
                span,
            );
            Ok(dst)
        }

        Expression::TemplateLiteral(t) if t.expressions.is_empty() && t.quasis.len() == 1 => {
            let quasi = &t.quasis[0];
            let cooked = quasi.value.cooked.as_deref().unwrap_or("");
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_string_constant(cooked);
            cx.emit(
                Op::LoadString,
                vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                (t.span.start, t.span.end),
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
                BinaryOperator::LessThan => Op::LessThan,
                BinaryOperator::LessEqualThan => Op::LessEq,
                BinaryOperator::GreaterThan => Op::GreaterThan,
                BinaryOperator::GreaterEqualThan => Op::GreaterEq,
                BinaryOperator::Instanceof => Op::Instanceof,
                other => {
                    return Err(CompileError::Unsupported {
                        node: format!("BinaryExpression ({other:?})"),
                        span,
                    });
                }
            };
            let dst = cx.alloc_scratch();
            cx.emit(
                op,
                vec![
                    Operand::Register(dst),
                    Operand::Register(lhs),
                    Operand::Register(rhs),
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
            // `Math.PI` / `Math.E` — lower to MathLoad so the runtime
            // doesn't need a real global object yet. Method-call
            // forms (`Math.abs(...)`) are handled in
            // `compile_method_call`.
            if let Expression::Identifier(id) = &m.object
                && id.name.as_str() == "Math"
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
            if let Expression::Identifier(id) = callee
                && id.name.as_str() == "Error"
            {
                return compile_error_construct(cx, &new_expr.arguments, new_span);
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
            let callee_reg = compile_expr(cx, callee, new_span)?;
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
                            // Hole: foundation slice fills with `undefined`.
                            let r = cx.alloc_scratch();
                            cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
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
                            let r = cx.alloc_scratch();
                            cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
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
                        if p.computed {
                            return Err(CompileError::Unsupported {
                                node: "ObjectExpression: computed key".to_string(),
                                span: key_span,
                            });
                        }
                        if !matches!(p.kind, oxc_ast::ast::PropertyKind::Init) {
                            return Err(CompileError::Unsupported {
                                node: "ObjectExpression: getter/setter".to_string(),
                                span: key_span,
                            });
                        }
                        let key_str = match &p.key {
                            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                                id.name.as_str().to_string()
                            }
                            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
                            _ => {
                                return Err(CompileError::Unsupported {
                                    node: "ObjectExpression: non-string property key".to_string(),
                                    span: key_span,
                                });
                            }
                        };
                        let value_reg = compile_expr(cx, &p.value, key_span)?;
                        let const_idx = cx.intern_string_constant(&key_str);
                        cx.emit(
                            Op::StoreProperty,
                            vec![
                                Operand::Register(dst),
                                Operand::ConstIndex(const_idx),
                                Operand::Register(value_reg),
                            ],
                            key_span,
                        );
                    }
                    oxc_ast::ast::ObjectPropertyKind::SpreadProperty(s) => {
                        return Err(CompileError::Unsupported {
                            node: "ObjectExpression: spread element".to_string(),
                            span: (s.span.start, s.span.end),
                        });
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
            let (function_id, captures) =
                compile_function(cx, &name, &f.params, &f.body, span, f.r#async)?;
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
            // The only legal MetaProperty inside a module is
            // `import.meta`. The runtime materialises it as a
            // JsObject the linker passes in as param 1; we hoist
            // it into `import_meta_uv` at function entry so
            // closures capture it.
            //
            // Spec: <https://tc39.es/ecma262/#prod-ImportMeta>
            //       <https://tc39.es/ecma262/#sec-meta-properties-runtime-semantics-evaluation>
            let span = (meta.span.start, meta.span.end);
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
            // Foundation: only literal-string `import("./x")` is
            // accepted. Non-literal specifiers raise
            // `MODULE_DYNAMIC_NON_LITERAL` (recorded in task 36a;
            // task 58 lifts the restriction with runtime-lazy
            // loading).
            //
            // Spec: <https://tc39.es/ecma262/#sec-import-call-runtime-semantics-evaluation>
            let span = (imp.span.start, imp.span.end);
            if cx.module_state.is_none() {
                return Err(CompileError::Unsupported {
                    node: "dynamic `import()` outside an ES-module fragment".to_string(),
                    span,
                });
            }
            let specifier = match unwrap_ts_expr(&imp.source) {
                Expression::StringLiteral(lit) => lit.value.as_str().to_string(),
                _ => {
                    return Err(CompileError::Unsupported {
                        node:
                            "MODULE_DYNAMIC_NON_LITERAL: non-literal `import(specifier)` argument"
                                .to_string(),
                        span,
                    });
                }
            };
            let ns_dst = cx.alloc_scratch();
            let spec_const = cx.intern_string_constant(&specifier);
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
    if s.r#await {
        return Err(CompileError::Unsupported {
            node: "ForOfStatement: for await".to_string(),
            span,
        });
    }

    // Identify the loop variable up front so we can complain
    // clearly if the head shape exceeds the foundation subset.
    let (binding_name, is_const) = match &s.left {
        oxc_ast::ast::ForStatementLeft::VariableDeclaration(decl) => {
            if matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var) {
                return Err(CompileError::Unsupported {
                    node: "ForOfStatement: `var` head".to_string(),
                    span,
                });
            }
            if decl.declarations.len() != 1 {
                return Err(CompileError::Unsupported {
                    node: "ForOfStatement: multi-declarator head".to_string(),
                    span,
                });
            }
            let declarator = &decl.declarations[0];
            let name = match &declarator.id {
                oxc_ast::ast::BindingPattern::BindingIdentifier(id) => id.name.as_str().to_string(),
                _ => {
                    return Err(CompileError::Unsupported {
                        node: "ForOfStatement: destructuring head".to_string(),
                        span,
                    });
                }
            };
            let is_const = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Const);
            (name, is_const)
        }
        _ => {
            return Err(CompileError::Unsupported {
                node: "ForOfStatement: assignment-target head (foundation requires `let`/`const`)"
                    .to_string(),
                span,
            });
        }
    };

    let iterable_reg = compile_expr(cx, &s.right, span)?;
    let iter_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetIterator,
        vec![Operand::Register(iter_reg), Operand::Register(iterable_reg)],
        span,
    );

    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();

    cx.loops.push(LoopFrame::default());
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

    // Per-iteration scope so `let x of …` rebinds fresh each pass.
    cx.enter_scope();
    let storage = cx.declare_binding(&binding_name, is_const, span)?;
    cx.emit_store_storage(value_reg, storage, span);
    cx.mark_initialized(&binding_name);
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
        let pname = match &param.pattern {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) => id.name.as_str().to_string(),
            _ => {
                return Err(CompileError::Unsupported {
                    node: "CatchClause: destructuring binding".to_string(),
                    span,
                });
            }
        };
        let storage = cx.declare_binding(&pname, false, span)?;
        cx.emit_store_storage(exc_reg, storage, span);
        cx.mark_initialized(&pname);
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
///   [`Op::BindFunction`] so the foundation interpreter avoids the
///   `CallMethodValue` dispatch overhead and rejects non-array
///   `apply` arguments at compile time when they're a literal.
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
    // Bare `Error("msg")` call (without `new`) is treated like
    // `new Error("msg")` per ES spec §20.5.1.1 — same lowering.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Error"
    {
        return compile_error_construct(cx, &call.arguments, span);
    }
    let has_spread = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    if has_spread {
        return compile_spread_call(cx, callee, &call.arguments, span);
    }
    if let Expression::StaticMemberExpression(member) = callee {
        // Foundation built-ins on the global `Object`: lower a few
        // canonical forms directly to dedicated opcodes so the
        // runtime does not need a host-callable bridge yet.
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Object"
        {
            let method = member.property.name.as_str();
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            return compile_object_builtin(cx, method, &arg_regs, span);
        }
        // `Math.<name>(args)` — lower through `Op::MathCall`. The
        // dispatcher resolves `<name>` against the namespace's
        // function table; constant-style names like `PI` reach the
        // method path only via a deliberate user `Math.PI()` call,
        // which surfaces as `UnknownIntrinsic`.
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Math"
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
            cx.emit(Op::MathCall, operands, span);
            return Ok(dst);
        }
        // `JSON.<name>(args)` — same shape as Math.
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "JSON"
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
            cx.emit(Op::JsonCall, operands, span);
            return Ok(dst);
        }
        // `Promise.<name>(args)` — same shape as Math / JSON.
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Promise"
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
            cx.emit(Op::PromiseCall, operands, span);
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
/// exactly once. `apply` requires its second argument (when
/// present) to be an array literal so the foundation can unpack it
/// into [`Op::CallWithThis`] without a runtime spread; dynamic
/// argument arrays surface as a `CompileError::Unsupported`
/// pointing the caller at the future spread / `apply` task.
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
                        other => {
                            return Err(CompileError::Unsupported {
                                node: format!(
                                    "Function.prototype.apply: dynamic args ({}); \
                                     foundation requires an array literal",
                                    expr_kind_name(other)
                                ),
                                span,
                            });
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
                if m.computed {
                    return Err(CompileError::Unsupported {
                        node: "ClassDeclaration: computed method key".to_string(),
                        span: (m.span.start, m.span.end),
                    });
                }
                if !matches!(
                    m.kind,
                    oxc_ast::ast::MethodDefinitionKind::Method
                        | oxc_ast::ast::MethodDefinitionKind::Constructor
                ) {
                    return Err(CompileError::Unsupported {
                        node: format!("ClassDeclaration: {:?} accessor", m.kind),
                        span: (m.span.start, m.span.end),
                    });
                }
                if matches!(m.kind, oxc_ast::ast::MethodDefinitionKind::Constructor) {
                    if ctor_method.is_some() {
                        return Err(CompileError::Unsupported {
                            node: "ClassDeclaration: multiple constructors".to_string(),
                            span: (m.span.start, m.span.end),
                        });
                    }
                    ctor_method = Some(m);
                }
            }
            oxc_ast::ast::ClassElement::PropertyDefinition(p) => {
                // Field declarations need TDZ + per-instance init
                // semantics that the foundation slice doesn't model.
                // Reject explicitly so users aren't surprised.
                return Err(CompileError::Unsupported {
                    node: "ClassDeclaration: field declaration (foundation supports methods only)"
                        .to_string(),
                    span: (p.span.start, p.span.end),
                });
            }
            oxc_ast::ast::ClassElement::AccessorProperty(p) => {
                return Err(CompileError::Unsupported {
                    node: "ClassDeclaration: accessor property".to_string(),
                    span: (p.span.start, p.span.end),
                });
            }
            oxc_ast::ast::ClassElement::StaticBlock(s) => {
                return Err(CompileError::Unsupported {
                    node: "ClassDeclaration: static initializer block".to_string(),
                    span: (s.span.start, s.span.end),
                });
            }
            oxc_ast::ast::ClassElement::TSIndexSignature(_) => {
                // TypeScript-only — erase silently.
            }
        }
    }

    // Compile the constructor body. When the user didn't write one,
    // synthesize the spec defaults: a base class gets an empty body,
    // a derived class gets `constructor(...args) { super(...args); }`.
    let display_name = class_name.unwrap_or("<class>").to_string();
    let (ctor_id, ctor_captures) = match ctor_method {
        Some(m) => compile_function(
            cx,
            &display_name,
            &m.value.params,
            &m.value.body,
            (m.span.start, m.span.end),
            m.value.r#async,
        )?,
        None => compile_synthetic_constructor(cx, &display_name, super_reg.is_some(), span)?,
    };

    let ctor_const = cx.intern_function_id(ctor_id);
    let ctor_reg = cx.alloc_scratch();
    emit_make_callable(cx, ctor_reg, ctor_const, &ctor_captures, false, span);

    // Install methods (instance + static) onto the right side.
    for element in &class.body.body {
        let oxc_ast::ast::ClassElement::MethodDefinition(m) = element else {
            continue;
        };
        if matches!(m.kind, oxc_ast::ast::MethodDefinitionKind::Constructor) {
            continue;
        }
        let method_span = (m.span.start, m.span.end);
        let method_name = match &m.key {
            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
            _ => {
                return Err(CompileError::Unsupported {
                    node: "ClassDeclaration: non-string method key".to_string(),
                    span: method_span,
                });
            }
        };
        let (m_id, m_captures) = compile_function(
            cx,
            &method_name,
            &m.value.params,
            &m.value.body,
            method_span,
            m.value.r#async,
        )?;
        let m_const = cx.intern_function_id(m_id);
        let m_reg = cx.alloc_scratch();
        emit_make_callable(cx, m_reg, m_const, &m_captures, false, method_span);
        let target_reg = if m.r#static {
            statics_reg
        } else {
            prototype_reg
        };
        let name_const = cx.intern_string_constant(&method_name);
        cx.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(target_reg),
                Operand::ConstIndex(name_const),
                Operand::Register(m_reg),
            ],
            method_span,
        );
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
) -> Result<(u32, Vec<u32>), CompileError> {
    let module = Rc::clone(&parent.top_mut().module);
    let child = FunctionContext::new(Rc::clone(&module));
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

/// Lower `new Error(arg)` / `Error(arg)` to [`Op::NewError`]. The
/// foundation slice supports the zero- and one-argument shapes
/// (the second `options` argument introduced by ES2022 is rejected
/// with a clear diagnostic).
fn compile_error_construct(
    cx: &mut Compiler,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    if arguments.len() > 1 {
        return Err(CompileError::Unsupported {
            node: "Error: more than one argument (foundation accepts only `message`)".to_string(),
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
                node: "Error: spread argument".to_string(),
                span: (s.span.start, s.span.end),
            });
        }
        Some(other) => compile_expr(cx, other.to_expression(), span)?,
    };
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::NewError,
        vec![Operand::Register(dst), Operand::Register(msg_reg)],
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
        _ => Err(CompileError::Unsupported {
            node: format!("Object.{method}/{}", arg_regs.len()),
            span,
        }),
    }
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
/// `TSInstantiationExpression` per ADR-0002 §4. Also unwraps
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

/// `true` for top-level TS statements that ADR-0002 §4 marks as
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

/// `Some((node, span))` for top-level TS statements that ADR-0002 §4
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
    /// foundation per ADR-0002 §4 (e.g., `enum`, runtime
    /// `namespace`, decorators).
    #[error("typescript construct {node} is not supported in foundation")]
    TypeScriptUnsupported {
        /// AST node kind name.
        node: String,
        /// Source span of the offending node.
        span: (u32, u32),
    },
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
    fn dynamic_import_with_non_literal_argument_is_rejected() {
        let src = "let s = \"./x.ts\"; import(s);";
        let parsed = parse(src, SyntaxSourceKind::TypeScript).unwrap();
        let err = compile_module_fragment(&parsed, &host_info(&[])).unwrap_err();
        match err {
            CompileError::Unsupported { node, .. } => {
                assert!(node.contains("MODULE_DYNAMIC_NON_LITERAL"), "got {node}");
            }
            other => panic!("expected MODULE_DYNAMIC_NON_LITERAL, got {other:?}"),
        }
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
