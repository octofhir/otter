//! ES-module graph builder + linker for the new engine.
//!
//! The graph driver:
//! 1. Walks the dependency graph from an entry specifier (BFS),
//!    parsing each `.ts` / `.js` file once and scanning it for
//!    static imports + literal-string `import("./x")` references.
//! 2. Compiles each module via [`otter_compiler::compile_module_fragment`]
//!    with a `ModuleHostInfo` carrying the pre-resolved
//!    `(specifier → target URL)` table for that file.
//! 3. Post-order DFS topological sort with cycle detection
//!    (catchable `RangeError`-shaped diagnostic). Hard depth cap
//!    `MODULE_DEPTH_LIMIT`.
//! 4. Linker — merges all module fragments into one
//!    [`BytecodeModule`] by rewriting `Constant::FunctionId`
//!    indices and concatenating function / constant tables.
//! 5. Synthesises an `<entry>` function that:
//!    - allocates a fresh `module_env` JsObject and `import_meta`
//!      JsObject per module (with `import_meta.url` populated);
//!    - registers each `(url, module_env)` into the
//!      [`Interpreter`]'s per-run module registry;
//!    - calls each `<module-init>` in post-order, passing
//!      `(module_env, import_meta)`.
//!
//! # Contents
//! - [`load_program`] — top-level entry that loads + links a graph
//!   rooted at a `.ts` / `.js` file path and returns the unified
//!   [`BytecodeModule`].
//! - [`GraphError`] — distinct error enum for graph-build failures.
//!
//! # Invariants
//! - Module URLs are canonical `file://` strings (the loader
//!   guarantees this).
//! - Each module is parsed and compiled exactly once per run.
//! - Linking is deterministic — modules visit in the order their
//!   imports first appear so the merged function table reflects
//!   source-graph topology.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-cyclic-module-records>
//!   — spec model for the cyclic-graph evaluation algorithm we
//!   approximate with post-order DFS + literal `import()`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use otter_bytecode::{
    BytecodeModule, Constant, Function, Instruction, ModuleInit, ModuleResolution, Op, Operand,
    SourceKind as BytecodeSourceKind, SpanEntry,
};
use otter_compiler::{
    CompileError, CompiledModuleMetadata, ModuleHostInfo, compile_module_fragment_to_module,
};
use otter_syntax::{Parsed, SourceKind, SyntaxError, parse};
use oxc_ast::ast::Expression;
use oxc_ast_visit::Visit;

use crate::module_loader::{LoaderError, ModuleLoader};

/// Hard cap on graph traversal depth. Cycles and absurdly deep
/// trees both surface here as a catchable `RangeError`-shaped
/// diagnostic before exhausting the host stack.
pub const MODULE_DEPTH_LIMIT: usize = 256;

/// Error variants for [`load_program`].
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum GraphError {
    /// Loader-side failure (resolve / load / extension).
    #[error("{0}")]
    Loader(#[from] LoaderError),
    /// Parse failed for a module.
    #[error("parse failed for `{url}`: {error}")]
    Parse {
        /// Module URL.
        url: String,
        /// Structured parser error.
        error: SyntaxError,
    },
    /// Compiler rejected the module fragment.
    #[error("compile failed for `{url}`: {error}")]
    Compile {
        /// Module URL.
        url: String,
        /// Structured compiler error.
        error: CompileError,
    },
    /// Cyclic-import detection / depth-limit hit.
    #[error("RangeError: module graph cycle or depth limit reached at `{url}`")]
    Cycle {
        /// URL where the cycle was detected.
        url: String,
    },
}

/// One loaded + compiled module fragment, plus its resolved
/// dependency edges.
#[derive(Debug)]
struct ModuleNode {
    /// Compiled fragment from `compile_module_fragment`.
    fragment: BytecodeModule,
    /// Compiler-owned metadata for this unlinked source module.
    metadata: CompiledModuleMetadata,
    /// `(specifier_text → target_url)` pairs, mirroring
    /// `fragment.module_resolutions` — used by the topological
    /// sort to walk to dependencies.
    deps: Vec<(String, String)>,
}

/// Key used by the module-set BTreeMap. Returns the fragment's
/// canonical module URL — the linker looks up nodes by URL.
fn nodes_key_for(fragment: &BytecodeModule) -> String {
    fragment.module.clone()
}

/// Parse `text` as a module and walk its AST gathering every
/// static-import / re-export source plus every literal-string
/// `import("./x")` specifier. Uses [`oxc_ast_visit::Visit`] so we
/// don't hand-roll match arms per AST node kind — adding new
/// expression / statement variants in OXC won't silently miss
/// dynamic-import calls hidden inside them.
///
/// Spec: <https://tc39.es/ecma262/#sec-imports>,
///       <https://tc39.es/ecma262/#sec-import-call-runtime-semantics-evaluation>.
fn collect_specifiers(parsed: &Parsed, url: &str) -> Result<Vec<String>, GraphError> {
    let program = parsed.program().map_err(|e| GraphError::Parse {
        url: url.to_string(),
        error: e,
    })?;
    let mut visitor = SpecifierVisitor::default();
    for stmt in &program.body {
        visitor.visit_statement(stmt);
    }
    Ok(visitor.out)
}

#[derive(Default)]
struct SpecifierVisitor {
    out: Vec<String>,
    seen: HashSet<String>,
}

impl SpecifierVisitor {
    fn record(&mut self, specifier: &str) {
        if self.seen.insert(specifier.to_string()) {
            self.out.push(specifier.to_string());
        }
    }
}

impl<'a> Visit<'a> for SpecifierVisitor {
    fn visit_import_declaration(&mut self, decl: &oxc_ast::ast::ImportDeclaration<'a>) {
        if !decl.import_kind.is_type() {
            self.record(decl.source.value.as_str());
        }
    }

    fn visit_export_named_declaration(&mut self, decl: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
        if !decl.export_kind.is_type()
            && let Some(src) = &decl.source
        {
            self.record(src.value.as_str());
        }
        // Walk into nested declarations / expressions so they
        // contribute their own dynamic imports (e.g. an exported
        // function body that calls `import("./x")`).
        oxc_ast_visit::walk::walk_export_named_declaration(self, decl);
    }

    fn visit_export_all_declaration(&mut self, decl: &oxc_ast::ast::ExportAllDeclaration<'a>) {
        if !decl.export_kind.is_type() {
            self.record(decl.source.value.as_str());
        }
    }

    fn visit_import_expression(&mut self, imp: &oxc_ast::ast::ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &imp.source {
            self.record(lit.value.as_str());
        }
        oxc_ast_visit::walk::walk_import_expression(self, imp);
    }
}

/// Load + parse + compile every module reachable from `entry_url`.
/// Returns the per-URL compiled fragments in the order they were
/// first seen by the BFS. Cycle detection uses a separate
/// "in-progress" set during the recursive resolve.
fn build_module_set(
    loader: &ModuleLoader,
    entry_url: String,
    entry_kind: SourceKind,
    entry_text: String,
) -> Result<BTreeMap<String, ModuleNode>, GraphError> {
    let mut nodes: BTreeMap<String, ModuleNode> = BTreeMap::new();
    let mut queue: Vec<(String, SourceKind, String)> = vec![(entry_url, entry_kind, entry_text)];
    let mut load_count = 0usize;
    while let Some((url, kind, text)) = queue.pop() {
        if nodes.contains_key(&url) {
            continue;
        }
        if loader.is_hosted_url(&url) {
            nodes.insert(
                url.clone(),
                ModuleNode {
                    fragment: hosted_module_fragment(&url),
                    metadata: CompiledModuleMetadata::default(),
                    deps: Vec::new(),
                },
            );
            continue;
        }
        load_count += 1;
        if load_count > MODULE_DEPTH_LIMIT {
            return Err(GraphError::Cycle { url });
        }
        let parsed = parse(text, kind).map_err(|e| GraphError::Parse {
            url: url.clone(),
            error: e,
        })?;
        let specifiers = collect_specifiers(&parsed, &url)?;
        let mut resolved_imports: HashMap<String, String> = HashMap::new();
        let mut deps: Vec<(String, String)> = Vec::with_capacity(specifiers.len());
        for spec in &specifiers {
            let target = loader.resolve(spec, Some(&url))?;
            resolved_imports.insert(spec.clone(), target.clone());
            deps.push((spec.clone(), target.clone()));
            if !nodes.contains_key(&target) {
                let loaded = loader.load(spec, Some(&url))?;
                queue.push((loaded.url, loaded.kind, loaded.text));
            }
        }
        let host = ModuleHostInfo {
            module_url: url.clone(),
            resolved_imports,
        };
        let compiled =
            compile_module_fragment_to_module(&parsed, &host).map_err(|e| GraphError::Compile {
                url: url.clone(),
                error: e,
            })?;
        let _ = url; // url is the BTreeMap key; ModuleNode itself doesn't need it
        nodes.insert(
            nodes_key_for(&compiled.bytecode),
            ModuleNode {
                fragment: compiled.bytecode,
                metadata: compiled.metadata,
                deps,
            },
        );
    }
    Ok(nodes)
}

fn hosted_module_fragment(url: &str) -> BytecodeModule {
    BytecodeModule {
        module: url.to_string(),
        source_kind: BytecodeSourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<hosted-module-init>".to_string(),
            param_count: 2,
            is_module: true,
            module_url: url.to_string(),
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: Vec::new(),
            }],
            spans: Vec::new(),
            ..Default::default()
        }],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// Topological sort of `nodes`, post-order DFS rooted at `entry`.
///
/// Cyclic edges are short-circuited per ECMA-262
/// §16.2.1.5 [HostLoadImportedModule] +
/// §16.2.1.6 [InnerModuleEvaluation]: when the DFS revisits a
/// module that is still on the in-progress stack, the back-edge
/// is skipped. Live-binding indirection through `module_env`
/// then keeps reads of late-bound exports correct at run time —
/// the skipped module reads observe the partially-populated env
/// at the moment they execute, exactly as the spec requires.
///
/// Iterative two-pass DFS to avoid recursion-depth concerns on
/// the host: each visit-frame on the work stack is
/// `(url, next-child-index)`. On finishing all children we emit
/// the URL into `order`.
///
/// [HostLoadImportedModule]: <https://tc39.es/ecma262/#sec-HostLoadImportedModule>
/// [InnerModuleEvaluation]: <https://tc39.es/ecma262/#sec-InnerModuleEvaluation>
fn topological_order(
    nodes: &BTreeMap<String, ModuleNode>,
    entry: &str,
) -> Result<Vec<String>, GraphError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        InProgress,
        Done,
    }
    let mut marks: HashMap<String, Mark> = HashMap::new();
    let mut order: Vec<String> = Vec::with_capacity(nodes.len());
    // Each stack entry: (url, next-child-index).
    let mut stack: Vec<(String, usize)> = vec![(entry.to_string(), 0)];
    marks.insert(entry.to_string(), Mark::InProgress);

    while let Some((url, child_idx)) = stack.last().cloned() {
        if stack.len() > MODULE_DEPTH_LIMIT {
            return Err(GraphError::Cycle { url });
        }
        let node = match nodes.get(&url) {
            Some(n) => n,
            None => {
                stack.pop();
                marks.insert(url.clone(), Mark::Done);
                order.push(url);
                continue;
            }
        };
        if child_idx >= node.deps.len() {
            stack.pop();
            marks.insert(url.clone(), Mark::Done);
            order.push(url);
            continue;
        }
        // Advance the parent's child index for the next iteration.
        if let Some(top) = stack.last_mut() {
            top.1 = child_idx + 1;
        }
        let (_, target) = &node.deps[child_idx];
        match marks.get(target).copied() {
            // Already emitted — the dependency's <module-init>
            // runs before the parent's, so nothing to do here.
            Some(Mark::Done) => continue,
            // Back-edge into a module that is still on the
            // DFS in-progress stack: the cyclic edge is skipped.
            // The dependency record is already allocated and its
            // env is reachable through `Op::ImportNamespace`, so
            // reads from the parent's body resolve through live-
            // binding indirection (a not-yet-populated export
            // simply reads as `undefined`, exactly per spec).
            Some(Mark::InProgress) => continue,
            None => {
                marks.insert(target.clone(), Mark::InProgress);
                stack.push((target.clone(), 0));
            }
        }
    }
    Ok(order)
}

/// Linker output: the merged [`BytecodeModule`] + the entry URL.
#[derive(Debug)]
pub struct LinkedProgram {
    /// Unified bytecode module. `<entry>` is at function id 0;
    /// every module's `<module-init>` follows at ids picked by
    /// the linker.
    pub module: BytecodeModule,
    /// Canonical URL of the entry module — useful for telemetry.
    pub entry_url: String,
    /// Per-source compiler metadata for linked modules before bytecode merge.
    pub metadata: Vec<CompiledModuleMetadata>,
}

/// Top-level entry: load the dependency graph rooted at `entry_path`,
/// compile every reachable module, and link them into one
/// [`LinkedProgram`] ready for the interpreter.
///
/// # Algorithm
/// 1. Load the entry source through `loader`.
/// 2. BFS to compile every reachable fragment via
///    [`compile_module_fragment`].
/// 3. Topologically sort with cycle detection.
/// 4. Link: assign function-ID and constant offsets per fragment;
///    rewrite each fragment's `Constant::FunctionId` operands
///    accordingly; concatenate functions / constants into one
///    table; populate `module_resolutions` and `module_inits`.
/// 5. Synthesise `<entry>` (function id 0) that allocates per-
///    module env / import_meta JsObjects, registers them with the
///    interpreter, and calls each `<module-init>` in post-order.
///
/// # Errors
/// See [`GraphError`].
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-link>
/// - <https://tc39.es/ecma262/#sec-cyclic-module-records>
pub fn load_program(loader: &ModuleLoader, entry_path: &Path) -> Result<LinkedProgram, GraphError> {
    // Read the entry directly so the user sees clear errors when
    // the entry path is malformed before any specifier-resolution
    // logic runs.
    let entry_kind =
        otter_syntax::detect_source_kind(entry_path).ok_or_else(|| LoaderError::Extension {
            url: format!("file://{}", entry_path.display()),
            extension: entry_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string(),
        })?;
    let entry_url = format!(
        "file://{}",
        std::fs::canonicalize(entry_path)
            .map_err(|e| LoaderError::Resolve {
                specifier: entry_path.display().to_string(),
                referrer: "<entry>".to_string(),
                message: e.to_string(),
            })?
            .display()
    );
    let entry_text = std::fs::read_to_string(entry_path).map_err(|e| LoaderError::Load {
        url: entry_url.clone(),
        message: e.to_string(),
    })?;

    let nodes = build_module_set(loader, entry_url.clone(), entry_kind, entry_text)?;
    let order = topological_order(&nodes, &entry_url)?;
    let module = link(&nodes, &order, &entry_url);
    let metadata = order
        .iter()
        .filter_map(|url| {
            nodes
                .get(url)
                .map(|node| node.metadata.clone())
                .filter(|metadata| !metadata.source_url.is_empty())
        })
        .collect();

    Ok(LinkedProgram {
        module,
        entry_url,
        metadata,
    })
}

/// Merge fragments into one `BytecodeModule`, prepending a
/// synthesised `<entry>` function (id 0) that drives module
/// initialisation.
fn link(nodes: &BTreeMap<String, ModuleNode>, order: &[String], entry_url: &str) -> BytecodeModule {
    // Reserve id 0 for `<entry>`.
    let mut functions: Vec<Function> = Vec::new();
    let mut constants: Vec<Constant> = Vec::new();
    // Per-module: offset to add to function IDs, offset to add
    // to constant indices.
    let mut module_function_offset: HashMap<String, u32> = HashMap::new();
    let mut module_inits: Vec<ModuleInit> = Vec::new();
    let mut module_resolutions: Vec<ModuleResolution> = Vec::new();

    // Placeholder `<entry>`; we'll fill in `code` + `spans`
    // after we know each fragment's offsets.
    functions.push(Function {
        id: 0,
        name: "<entry>".to_string(),
        ..Default::default()
    });

    // First pass: append every fragment's functions + constants
    // with offsets recorded so we can rewrite cross-references.
    for url in order {
        let Some(node) = nodes.get(url) else {
            continue;
        };
        let fn_offset = functions.len() as u32;
        let const_offset = constants.len() as u32;
        module_function_offset.insert(url.clone(), fn_offset);

        // Append constants, rewriting `FunctionId` indices.
        for c in &node.fragment.constants {
            let rewritten = match c {
                Constant::FunctionId { index } => Constant::FunctionId {
                    index: index + fn_offset,
                },
                other => other.clone(),
            };
            constants.push(rewritten);
        }

        // Append functions, rewriting their constant operands +
        // function IDs.
        for f in &node.fragment.functions {
            let mut new_fn = f.clone();
            new_fn.id = fn_offset + f.id;
            new_fn.code = rewrite_const_indices(&f.code, const_offset);
            functions.push(new_fn);
        }

        // Module-init goes at fn_offset + 0 (id 0 of the fragment).
        module_inits.push(ModuleInit {
            url: url.clone(),
            function_id: fn_offset,
        });

        // Carry forward every resolution edge, so the runtime's
        // ImportNamespace dispatcher can match on
        // (referrer, specifier, target).
        for edge in &node.fragment.module_resolutions {
            module_resolutions.push(edge.clone());
        }
    }

    // Synthesise <entry>'s body. Ordering: for each module, build
    // module_env + import_meta, register, then call its <module-init>.
    let entry_body = build_entry_body(nodes, order, &module_function_offset, &mut constants);
    functions[0].code = entry_body.code;
    functions[0].spans = entry_body.spans;
    functions[0].locals = 0;
    functions[0].scratch = entry_body.scratch;
    functions[0].param_count = 0;
    functions[0].own_upvalue_count = 0;

    BytecodeModule {
        module: entry_url.to_string(),
        source_kind: BytecodeSourceKind::TypeScript,
        functions,
        constants,
        module_resolutions,
        module_inits,
    }
}

/// Rewrite per-fragment constant-pool references after the
/// linker concatenates fragments. Adds `offset` to every
/// [`Operand::ConstIndex`] operand whose opcode/position pair
/// actually indexes the constant pool, per
/// [`Op::is_const_pool_operand`]. Other [`Operand::ConstIndex`]
/// uses (`argc`, `upvalue_count`, method-id enums, typed-array
/// kind enums, …) are intentionally left untouched.
fn rewrite_const_indices(code: &[Instruction], offset: u32) -> Vec<Instruction> {
    code.iter()
        .map(|instr| {
            let op = instr.op;
            Instruction {
                pc: instr.pc,
                op,
                operands: instr
                    .operands
                    .iter()
                    .enumerate()
                    .map(|(pos, operand)| match operand {
                        Operand::ConstIndex(k) if op.is_const_pool_operand(pos) => {
                            Operand::ConstIndex(k + offset)
                        }
                        other => other.clone(),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// One assembled `<entry>` body.
struct EntryBody {
    code: Vec<Instruction>,
    spans: Vec<SpanEntry>,
    scratch: u16,
}

/// Build the synthesised `<entry>` driver. Pseudocode:
///
/// ```ignore
/// // For each module in post-order:
/// const module_env_<i> = NewObject;
/// const import_meta_<i> = NewObject;
/// import_meta_<i>.url = "<canonical url>";
/// __register_module_env("<canonical url>", module_env_<i>);
/// MakeFunction r_init, k[init_const_<i>];
/// CallWithThis _, r_init, undefined, [module_env_<i>, import_meta_<i>];
/// ```
///
/// The `__register_module_env` step is **not** an emitted opcode
/// — we cannot synthesise a JS-level call into Rust. Instead the
/// runtime walks `module.module_inits` once at run start and
/// pre-allocates the `module_env` JsObjects keyed by URL; the
/// `<entry>` body just looks them up via [`Op::ImportNamespace`]
/// (which already resolves `(referrer, specifier) → module_env`
/// via the module-resolutions table). So the actual emitted
/// shape is much simpler:
///
/// ```ignore
/// // <entry> sees itself as the referrer for every module's URL.
/// // The linker pre-populates module_resolutions with self-loops
/// // so ImportNamespace("<url>") returns the right env.
/// const module_env_<i> = ImportNamespace k["<url>"];
/// const import_meta_<i> = NewObject;
/// import_meta_<i>.url = "<canonical url>";
/// MakeFunction r_init, k[init_const_<i>];
/// CallWithThis _, r_init, undefined, [module_env_<i>, import_meta_<i>];
/// ```
///
/// Pre-population happens in
/// [`Interpreter::register_module_env`](otter_vm::Interpreter::register_module_env)
/// invoked by [`crate::Runtime::run_module_program`] before
/// dispatching the entry.
fn build_entry_body(
    nodes: &BTreeMap<String, ModuleNode>,
    order: &[String],
    module_function_offset: &HashMap<String, u32>,
    constants: &mut Vec<Constant>,
) -> EntryBody {
    let mut code: Vec<Instruction> = Vec::new();
    let mut spans: Vec<SpanEntry> = Vec::new();
    let mut next_pc: u32 = 0;
    let mut next_reg: u16 = 0;

    let url_name_idx = intern_string_const(constants, "url");

    for url in order {
        let Some(_node) = nodes.get(url) else {
            continue;
        };
        let init_fn_id = match module_function_offset.get(url) {
            Some(&id) => id,
            None => continue,
        };

        // r_env: the module's env JsObject (resolved via the
        // pre-populated registry against the <entry> referrer
        // through the self-loop module_resolutions edge).
        let r_env = next_reg;
        next_reg += 1;
        let url_const_idx = intern_string_const(constants, url);
        emit_op(
            &mut code,
            &mut spans,
            &mut next_pc,
            Op::ImportNamespace,
            vec![Operand::Register(r_env), Operand::ConstIndex(url_const_idx)],
        );

        // r_meta = new JsObject; r_meta.url = url.
        let r_meta = next_reg;
        next_reg += 1;
        emit_op(
            &mut code,
            &mut spans,
            &mut next_pc,
            Op::NewObject,
            vec![Operand::Register(r_meta)],
        );

        let r_url_str = next_reg;
        next_reg += 1;
        emit_op(
            &mut code,
            &mut spans,
            &mut next_pc,
            Op::LoadString,
            vec![
                Operand::Register(r_url_str),
                Operand::ConstIndex(url_const_idx),
            ],
        );
        let r_store_scratch = next_reg;
        next_reg += 1;
        emit_op(
            &mut code,
            &mut spans,
            &mut next_pc,
            Op::StoreProperty,
            vec![
                Operand::Register(r_meta),
                Operand::ConstIndex(url_name_idx),
                Operand::Register(r_url_str),
                Operand::Register(r_store_scratch),
            ],
        );

        // r_init = MakeFunction k[init_fn_id]
        let r_init = next_reg;
        next_reg += 1;
        let init_const_idx = intern_function_id_const(constants, init_fn_id);
        emit_op(
            &mut code,
            &mut spans,
            &mut next_pc,
            Op::MakeFunction,
            vec![
                Operand::Register(r_init),
                Operand::ConstIndex(init_const_idx),
            ],
        );

        // r_dummy = CallWithThis r_init, undefined, [r_env, r_meta]
        let r_dummy = next_reg;
        next_reg += 1;
        let r_undef = next_reg;
        next_reg += 1;
        emit_op(
            &mut code,
            &mut spans,
            &mut next_pc,
            Op::LoadUndefined,
            vec![Operand::Register(r_undef)],
        );
        emit_op(
            &mut code,
            &mut spans,
            &mut next_pc,
            Op::CallWithThis,
            vec![
                Operand::Register(r_dummy),
                Operand::Register(r_init),
                Operand::Register(r_undef),
                Operand::ConstIndex(2),
                Operand::Register(r_env),
                Operand::Register(r_meta),
            ],
        );
    }

    emit_op(
        &mut code,
        &mut spans,
        &mut next_pc,
        Op::ReturnUndefined,
        vec![],
    );
    EntryBody {
        code,
        spans,
        scratch: next_reg,
    }
}

fn emit_op(
    code: &mut Vec<Instruction>,
    spans: &mut Vec<SpanEntry>,
    next_pc: &mut u32,
    op: Op,
    operands: Vec<Operand>,
) {
    let pc = *next_pc;
    code.push(Instruction { pc, op, operands });
    spans.push(SpanEntry { pc, span: (0, 0) });
    *next_pc += 1;
}

fn intern_string_const(constants: &mut Vec<Constant>, s: &str) -> u32 {
    let target: Vec<u16> = s.encode_utf16().collect();
    for (i, c) in constants.iter().enumerate() {
        if let Constant::String { utf16 } = c
            && utf16 == &target
        {
            return i as u32;
        }
    }
    constants.push(Constant::String { utf16: target });
    (constants.len() - 1) as u32
}

fn intern_function_id_const(constants: &mut Vec<Constant>, function_id: u32) -> u32 {
    for (i, c) in constants.iter().enumerate() {
        if let Constant::FunctionId { index } = c
            && *index == function_id
        {
            return i as u32;
        }
    }
    constants.push(Constant::FunctionId { index: function_id });
    (constants.len() - 1) as u32
}
