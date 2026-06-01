//! ES-module graph builder + linker for the new engine.
//!
//! The graph driver:
//! 1. Walks the dependency graph from an entry specifier (BFS),
//!    parsing each `.ts` / `.js` file once and scanning it for
//!    static imports + literal-string `import("./x")` references.
//! 2. Compiles each module via [`otter_compiler::compile_module_program_to_module`]
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
//! - `ModuleGraphBuilder -> ModuleGraph -> LinkedProgram` — transient graph
//!   discovery followed by frozen linked output.
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
    OperandList, SourceKind as BytecodeSourceKind, SpanEntry,
};
use otter_compiler::{
    CompileError, CompiledModuleMetadata, ModuleHostInfo, compile_module_program_to_module,
};
use otter_syntax::{SourceKind, SyntaxError, with_program};
use oxc_ast::ast::{Expression, Program};
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
    /// Compiled fragment from `compile_module_program_to_module`.
    fragment: BytecodeModule,
    /// Compiler-owned metadata for this unlinked source module.
    metadata: CompiledModuleMetadata,
    /// Source-ordered module requests, preserving `defer` phase
    /// records even when the same specifier also appears eagerly.
    deps: Vec<ModuleEdge>,
}

#[derive(Debug)]
struct ModuleEdge {
    target: String,
    deferred: bool,
}

/// Frozen module graph assembled by [`ModuleGraphBuilder`].
#[derive(Debug)]
struct ModuleGraph {
    entry_url: String,
    nodes: BTreeMap<String, ModuleNode>,
}

impl ModuleGraph {
    fn link(self) -> Result<LinkedProgram, GraphError> {
        let order = topological_order(&self.nodes, &self.entry_url)?;
        let module = link(&self.nodes, &order, &self.entry_url);
        let metadata = order
            .iter()
            .filter_map(|url| {
                self.nodes
                    .get(url)
                    .map(|node| node.metadata.clone())
                    .filter(|metadata| !metadata.source_url.is_empty())
            })
            .collect();

        Ok(LinkedProgram {
            module,
            entry_url: self.entry_url,
            metadata,
        })
    }
}

/// Transient dependency-graph builder.
///
/// Owns BFS queueing, per-run load counters, and mutable node insertion while
/// the graph is discovered. Once built, callers receive a frozen [`ModuleGraph`]
/// and linking no longer mutates graph topology.
struct ModuleGraphBuilder<'a> {
    loader: &'a ModuleLoader,
    entry_url: String,
    nodes: BTreeMap<String, ModuleNode>,
    queue: Vec<(String, SourceKind, String, bool)>,
    load_count: usize,
}

impl<'a> ModuleGraphBuilder<'a> {
    fn new(
        loader: &'a ModuleLoader,
        entry_url: String,
        entry_kind: SourceKind,
        entry_text: String,
    ) -> Self {
        Self {
            loader,
            entry_url: entry_url.clone(),
            nodes: BTreeMap::new(),
            queue: vec![(entry_url, entry_kind, entry_text, false)],
            load_count: 0,
        }
    }

    fn build(mut self) -> Result<ModuleGraph, GraphError> {
        while let Some((url, kind, text, dynamic)) = self.queue.pop() {
            if let Err(err) = self.load_one(url, kind, text, dynamic) {
                if dynamic {
                    continue;
                }
                return Err(err);
            }
        }
        Ok(ModuleGraph {
            entry_url: self.entry_url,
            nodes: self.nodes,
        })
    }

    fn load_one(
        &mut self,
        url: String,
        kind: SourceKind,
        text: String,
        optional_dynamic: bool,
    ) -> Result<(), GraphError> {
        if self.nodes.contains_key(&url) {
            return Ok(());
        }
        if self.loader.is_hosted_url(&url) {
            self.nodes.insert(
                url.clone(),
                ModuleNode {
                    fragment: hosted_module_fragment(&url),
                    metadata: CompiledModuleMetadata::default(),
                    deps: Vec::new(),
                },
            );
            return Ok(());
        }
        self.load_count += 1;
        if self.load_count > MODULE_DEPTH_LIMIT {
            return Err(GraphError::Cycle { url });
        }

        let (compiled, deps, queued) = with_program(text, kind, |program| {
            let requests = collect_module_requests(program);
            let mut resolved_imports: HashMap<String, String> = HashMap::new();
            let mut deps: Vec<ModuleEdge> = Vec::with_capacity(requests.len());
            let mut queued: Vec<(String, SourceKind, String, bool)> = Vec::new();
            let mut eager_static_specs: HashSet<String> = HashSet::new();
            let mut dynamic_specs: HashSet<String> = HashSet::new();
            for request in &requests {
                let target = self.loader.resolve(&request.specifier, Some(&url))?;
                resolved_imports.insert(request.specifier.clone(), target.clone());
                if request.dynamic {
                    dynamic_specs.insert(request.specifier.clone());
                } else if !request.deferred {
                    eager_static_specs.insert(request.specifier.clone());
                }
                deps.push(ModuleEdge {
                    target: target.clone(),
                    deferred: request.deferred,
                });
                if !self.nodes.contains_key(&target) {
                    let loaded = self.loader.load(&request.specifier, Some(&url))?;
                    queued.push((
                        loaded.url,
                        loaded.kind,
                        loaded.text,
                        optional_dynamic || request.dynamic,
                    ));
                }
            }
            let host = ModuleHostInfo {
                module_url: url.clone(),
                resolved_imports,
            };
            let mut compiled =
                compile_module_program_to_module(program, kind, &host).map_err(|e| {
                    GraphError::Compile {
                        url: url.clone(),
                        error: e,
                    }
                })?;
            for edge in &mut compiled.bytecode.module_resolutions {
                if dynamic_specs.contains(&edge.specifier)
                    && !eager_static_specs.contains(&edge.specifier)
                {
                    edge.deferred = true;
                }
            }
            Ok::<_, GraphError>((compiled, deps, queued))
        })
        .map_err(|e| GraphError::Parse {
            url: url.clone(),
            error: e,
        })??;

        self.queue.extend(queued);
        self.nodes.insert(
            nodes_key_for(&compiled.bytecode),
            ModuleNode {
                fragment: compiled.bytecode,
                metadata: compiled.metadata,
                deps,
            },
        );
        Ok(())
    }
}

/// Key used by the module-set BTreeMap. Returns the fragment's
/// canonical module URL — the linker looks up nodes by URL.
fn nodes_key_for(fragment: &BytecodeModule) -> String {
    fragment.module.clone()
}

/// Parse `text` as a module and walk its AST gathering every
/// static-import / re-export source plus every literal-string
/// `import("./x")` specifier. Static imports preserve phase in source
/// order so `import defer "x"` and later eager `import "x"` remain
/// distinct module requests for evaluation ordering.
///
/// Spec: <https://tc39.es/ecma262/#sec-imports>,
///       <https://tc39.es/ecma262/#sec-import-call-runtime-semantics-evaluation>.
fn collect_module_requests(program: &Program<'_>) -> Vec<ModuleRequest> {
    let mut visitor = ModuleRequestVisitor::default();
    for stmt in &program.body {
        visitor.visit_statement(stmt);
    }
    visitor.out
}

#[derive(Default)]
struct ModuleRequestVisitor {
    out: Vec<ModuleRequest>,
    seen: HashSet<(String, bool)>,
}

#[derive(Clone)]
struct ModuleRequest {
    specifier: String,
    deferred: bool,
    dynamic: bool,
}

impl ModuleRequestVisitor {
    fn record(&mut self, specifier: &str, deferred: bool, dynamic: bool) {
        let key = (specifier.to_string(), deferred);
        if self.seen.insert(key) {
            self.out.push(ModuleRequest {
                specifier: specifier.to_string(),
                deferred,
                dynamic,
            });
        }
    }
}

impl<'a> Visit<'a> for ModuleRequestVisitor {
    fn visit_import_declaration(&mut self, decl: &oxc_ast::ast::ImportDeclaration<'a>) {
        if !decl.import_kind.is_type() {
            self.record(
                decl.source.value.as_str(),
                matches!(decl.phase, Some(oxc_ast::ast::ImportPhase::Defer)),
                false,
            );
        }
    }

    fn visit_export_named_declaration(&mut self, decl: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
        if !decl.export_kind.is_type()
            && let Some(src) = &decl.source
        {
            self.record(src.value.as_str(), false, false);
        }
        // Walk into nested declarations / expressions so they
        // contribute their own dynamic imports (e.g. an exported
        // function body that calls `import("./x")`).
        oxc_ast_visit::walk::walk_export_named_declaration(self, decl);
    }

    fn visit_export_all_declaration(&mut self, decl: &oxc_ast::ast::ExportAllDeclaration<'a>) {
        if !decl.export_kind.is_type() {
            self.record(decl.source.value.as_str(), false, false);
        }
    }

    fn visit_import_expression(&mut self, imp: &oxc_ast::ast::ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &imp.source {
            let specifier = lit.value.as_str();
            if dynamic_literal_should_preload(specifier) {
                self.record(specifier, true, true);
            }
        }
        oxc_ast_visit::walk::walk_import_expression(self, imp);
    }
}

fn dynamic_literal_should_preload(specifier: &str) -> bool {
    specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
        || specifier.starts_with("file://")
        || specifier.starts_with("http://")
        || specifier.starts_with("https://")
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
                operands: Vec::new().into(),
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

    let mut async_eval: HashSet<&str> = nodes
        .iter()
        .filter_map(|(url, node)| {
            node.fragment
                .functions
                .first()
                .is_some_and(|f| f.is_async)
                .then_some(url.as_str())
        })
        .collect();
    loop {
        let mut changed = false;
        for node in nodes.values() {
            for edge in &node.fragment.module_resolutions {
                if !edge.deferred
                    && async_eval.contains(edge.target.as_str())
                    && async_eval.insert(edge.referrer.as_str())
                {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut roots = Vec::with_capacity(nodes.len() + 1);
    roots.push(entry.to_string());
    roots.extend(nodes.keys().filter(|url| url.as_str() != entry).cloned());

    for root in roots {
        if marks.contains_key(&root) {
            continue;
        }
        // Each stack entry: (url, next-child-index).
        let mut stack: Vec<(String, usize)> = vec![(root.clone(), 0)];
        marks.insert(root, Mark::InProgress);

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
            let edge = &node.deps[child_idx];
            if edge.deferred && !async_eval.contains(edge.target.as_str()) {
                continue;
            }
            let target = &edge.target;
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
///    [`compile_module_program_to_module`].
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

    ModuleGraphBuilder::new(loader, entry_url, entry_kind, entry_text)
        .build()?
        .link()
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
            if new_fn.module_url.is_empty() {
                new_fn.module_url = url.clone();
            }
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
    let entry_body = build_entry_body(
        nodes,
        order,
        entry_url,
        &module_function_offset,
        &mut constants,
    );
    functions[0].code = entry_body.code;
    functions[0].spans = entry_body.spans;
    functions[0].locals = 0;
    functions[0].scratch = entry_body.scratch;
    functions[0].param_count = 0;
    functions[0].own_upvalue_count = 0;
    // A graph that contains a top-level-await module evaluates through an
    // async `<entry>` that awaits each module-init; mark it async so the
    // dispatch loop parks/resumes it via the microtask queue.
    functions[0].is_async = entry_body.is_async;

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
                    .map(|(pos, operand)| match *operand {
                        Operand::ConstIndex(k) if op.is_const_pool_operand(pos) => {
                            Operand::ConstIndex(k + offset)
                        }
                        other => other,
                    })
                    .collect::<Vec<_>>()
                    .into(),
            }
        })
        .collect()
}

/// One assembled `<entry>` body.
struct EntryBody {
    code: Vec<Instruction>,
    spans: Vec<SpanEntry>,
    scratch: u16,
    /// `true` when the graph contains a top-level-await module, so the
    /// `<entry>` driver must be async and await each module-init.
    is_async: bool,
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
    entry_url: &str,
    module_function_offset: &HashMap<String, u32>,
    constants: &mut Vec<Constant>,
) -> EntryBody {
    let mut code: Vec<Instruction> = Vec::new();
    let mut spans: Vec<SpanEntry> = Vec::new();
    let mut next_pc: u32 = 0;

    // A module-init has its own top-level await iff its `<main>`
    // (fragment function 0) is async.
    let module_has_tla = |url: &str| -> bool {
        nodes
            .get(url)
            .and_then(|n| n.fragment.functions.first())
            .is_some_and(|f| f.is_async)
    };

    // [[AsyncEvaluation]] set (§16.2.1.5): a module is async-evaluated if
    // it has top-level await or any of its non-deferred dependencies is
    // async-evaluated. Computed as a fixpoint over non-deferred edges
    // (deferred edges evaluate their target as a separate root).
    let mut async_eval: HashSet<&str> = order
        .iter()
        .map(String::as_str)
        .filter(|u| module_has_tla(u))
        .collect();
    loop {
        let mut changed = false;
        for node in nodes.values() {
            for edge in &node.fragment.module_resolutions {
                if !edge.deferred
                    && async_eval.contains(edge.target.as_str())
                    && async_eval.insert(edge.referrer.as_str())
                {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let module_is_async = |url: &str| async_eval.contains(url);

    // Eagerly-reachable module set: BFS from the entry module over
    // non-deferred resolution edges. Modules reachable only through
    // `import defer` edges are evaluated lazily on first namespace
    // access (TC39 import defer), so the eager driver skips them — except
    // when the deferred target is async-evaluated: it cannot be
    // force-evaluated synchronously, so per the proposal it is evaluated
    // eagerly and the deferred namespace wraps the already-evaluated
    // module.
    let non_deferred_children = |url: &str| -> Vec<String> {
        nodes
            .get(url)
            .map(|node| {
                node.fragment
                    .module_resolutions
                    .iter()
                    .filter(|edge| !edge.deferred)
                    .map(|edge| edge.target.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let collect_tla_descendants = |root: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![root.to_string()];
        while let Some(url) = stack.pop() {
            if !seen.insert(url.clone()) {
                continue;
            }
            if module_has_tla(&url) {
                out.push(url);
                continue;
            }
            stack.extend(non_deferred_children(&url));
        }
        out
    };

    let mut adjacency: HashMap<&str, Vec<String>> = HashMap::new();
    let mut deferred_async_modules: HashSet<String> = HashSet::new();
    for node in nodes.values() {
        for edge in &node.fragment.module_resolutions {
            if !edge.deferred {
                adjacency
                    .entry(edge.referrer.as_str())
                    .or_default()
                    .push(edge.target.clone());
            } else {
                let async_roots = collect_tla_descendants(edge.target.as_str());
                if !async_roots.is_empty() {
                    deferred_async_modules.extend(async_roots.iter().cloned());
                    adjacency
                        .entry(edge.referrer.as_str())
                        .or_default()
                        .extend(async_roots);
                }
            }
        }
    }
    let mut reachable: HashSet<String> = HashSet::new();
    let mut work = vec![entry_url.to_string()];
    while let Some(url) = work.pop() {
        if !reachable.insert(url.clone()) {
            continue;
        }
        if let Some(deps) = adjacency.get(url.as_str()) {
            work.extend(deps.iter().cloned());
        }
    }

    let has_async = order
        .iter()
        .any(|url| reachable.contains(url) && module_is_async(url));

    if !has_async {
        // Sync graph: one idempotent EvaluateModule per eagerly-reachable
        // module in topological post-order. Unchanged fast path.
        for url in order {
            if !reachable.contains(url) {
                continue;
            }
            let url_const_idx = intern_string_const(constants, url);
            emit_op(
                &mut code,
                &mut spans,
                &mut next_pc,
                Op::EvaluateModule,
                [Operand::ConstIndex(url_const_idx)],
            );
        }
        emit_op(&mut code, &mut spans, &mut next_pc, Op::ReturnUndefined, []);
        return EntryBody {
            code,
            spans,
            scratch: 0,
            is_async: false,
        };
    }

    // Async graph (§16.2.1.5): the `<entry>` is an async function that,
    // for each eagerly-reachable module in post-order, builds its env +
    // import_meta, marks it evaluated, calls its `<module-init>`, then
    // awaits the result. A synchronous init runs to completion inline
    // (await of `undefined` is a microtask no-op); an async (top-level
    // await) init parks the entry until its body settles, so a module's
    // dependencies — which precede it in `order` — are fully evaluated
    // before it runs.
    let url_name_idx = intern_string_const(constants, "url");
    let mut next_reg: u16 = 0;
    let mut deferred_pending: Vec<(u32, u16)> = Vec::new();
    let mut deferred_postponed: Vec<String> = Vec::new();
    for url in order {
        if !reachable.contains(url) {
            continue;
        }
        if url == entry_url {
            for (pending_url_const_idx, pending_reg) in deferred_pending.drain(..) {
                emit_await_module_result(
                    &mut code,
                    &mut spans,
                    &mut next_pc,
                    &mut next_reg,
                    pending_url_const_idx,
                    pending_reg,
                );
            }
            for postponed_url in deferred_postponed.drain(..) {
                let Some(&postponed_init_fn_id) = module_function_offset.get(&postponed_url) else {
                    continue;
                };
                let (postponed_url_const_idx, postponed_result_reg) = emit_module_init_call(
                    &mut code,
                    &mut spans,
                    &mut next_pc,
                    &mut next_reg,
                    constants,
                    url_name_idx,
                    &postponed_url,
                    postponed_init_fn_id,
                );
                emit_await_module_result(
                    &mut code,
                    &mut spans,
                    &mut next_pc,
                    &mut next_reg,
                    postponed_url_const_idx,
                    postponed_result_reg,
                );
            }
        }
        if module_is_async(url)
            && deferred_async_modules.contains(url)
            && url != entry_url
            && nodes.get(url).is_some_and(|node| {
                node.fragment
                    .module_resolutions
                    .iter()
                    .any(|edge| !edge.deferred && deferred_async_modules.contains(&edge.target))
            })
        {
            deferred_postponed.push(url.clone());
            continue;
        }
        let Some(&init_fn_id) = module_function_offset.get(url) else {
            continue;
        };
        let (url_const_idx, result_reg) = emit_module_init_call(
            &mut code,
            &mut spans,
            &mut next_pc,
            &mut next_reg,
            constants,
            url_name_idx,
            url,
            init_fn_id,
        );
        if module_is_async(url) && deferred_async_modules.contains(url) && url != entry_url {
            deferred_pending.push((url_const_idx, result_reg));
        } else if module_is_async(url) {
            emit_await_module_result(
                &mut code,
                &mut spans,
                &mut next_pc,
                &mut next_reg,
                url_const_idx,
                result_reg,
            );
        } else {
            emit_op(
                &mut code,
                &mut spans,
                &mut next_pc,
                Op::MarkModuleEvaluated,
                [Operand::ConstIndex(url_const_idx)],
            );
        }
    }
    for (pending_url_const_idx, pending_reg) in deferred_pending {
        emit_await_module_result(
            &mut code,
            &mut spans,
            &mut next_pc,
            &mut next_reg,
            pending_url_const_idx,
            pending_reg,
        );
    }
    for postponed_url in deferred_postponed {
        let Some(&postponed_init_fn_id) = module_function_offset.get(&postponed_url) else {
            continue;
        };
        let (postponed_url_const_idx, postponed_result_reg) = emit_module_init_call(
            &mut code,
            &mut spans,
            &mut next_pc,
            &mut next_reg,
            constants,
            url_name_idx,
            &postponed_url,
            postponed_init_fn_id,
        );
        emit_await_module_result(
            &mut code,
            &mut spans,
            &mut next_pc,
            &mut next_reg,
            postponed_url_const_idx,
            postponed_result_reg,
        );
    }

    emit_op(&mut code, &mut spans, &mut next_pc, Op::ReturnUndefined, []);
    EntryBody {
        code,
        spans,
        scratch: next_reg,
        is_async: true,
    }
}

fn emit_module_init_call(
    code: &mut Vec<Instruction>,
    spans: &mut Vec<SpanEntry>,
    next_pc: &mut u32,
    next_reg: &mut u16,
    constants: &mut Vec<Constant>,
    url_name_idx: u32,
    url: &str,
    init_fn_id: u32,
) -> (u32, u16) {
    let url_const_idx = intern_string_const(constants, url);

    let r_env = *next_reg;
    *next_reg += 1;
    emit_op(
        code,
        spans,
        next_pc,
        Op::ImportNamespace,
        [Operand::Register(r_env), Operand::ConstIndex(url_const_idx)],
    );

    let r_meta = *next_reg;
    *next_reg += 1;
    emit_op(
        code,
        spans,
        next_pc,
        Op::NewObject,
        [Operand::Register(r_meta)],
    );
    let r_url_str = *next_reg;
    *next_reg += 1;
    emit_op(
        code,
        spans,
        next_pc,
        Op::LoadString,
        [
            Operand::Register(r_url_str),
            Operand::ConstIndex(url_const_idx),
        ],
    );
    let r_store_scratch = *next_reg;
    *next_reg += 1;
    emit_op(
        code,
        spans,
        next_pc,
        Op::StoreProperty,
        vec![
            Operand::Register(r_meta),
            Operand::ConstIndex(url_name_idx),
            Operand::Register(r_url_str),
            Operand::Register(r_store_scratch),
        ],
    );

    let r_init = *next_reg;
    *next_reg += 1;
    let init_const_idx = intern_function_id_const(constants, init_fn_id);
    emit_op(
        code,
        spans,
        next_pc,
        Op::MakeFunction,
        [
            Operand::Register(r_init),
            Operand::ConstIndex(init_const_idx),
        ],
    );

    let r_undef = *next_reg;
    *next_reg += 1;
    emit_op(
        code,
        spans,
        next_pc,
        Op::LoadUndefined,
        [Operand::Register(r_undef)],
    );
    let r_res = *next_reg;
    *next_reg += 1;
    emit_op(
        code,
        spans,
        next_pc,
        Op::CallWithThis,
        vec![
            Operand::Register(r_res),
            Operand::Register(r_init),
            Operand::Register(r_undef),
            Operand::ConstIndex(2),
            Operand::Register(r_env),
            Operand::Register(r_meta),
        ],
    );
    (url_const_idx, r_res)
}

fn emit_await_module_result(
    code: &mut Vec<Instruction>,
    spans: &mut Vec<SpanEntry>,
    next_pc: &mut u32,
    next_reg: &mut u16,
    url_const_idx: u32,
    result_reg: u16,
) {
    let r_awaited = *next_reg;
    *next_reg += 1;
    emit_op(
        code,
        spans,
        next_pc,
        Op::Await,
        [Operand::Register(r_awaited), Operand::Register(result_reg)],
    );
    emit_op(
        code,
        spans,
        next_pc,
        Op::MarkModuleEvaluated,
        [Operand::ConstIndex(url_const_idx)],
    );
}

/// Intern a `Constant::FunctionId` and return its constant-pool index.
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

fn emit_op(
    code: &mut Vec<Instruction>,
    spans: &mut Vec<SpanEntry>,
    next_pc: &mut u32,
    op: Op,
    operands: impl Into<OperandList>,
) {
    let pc = *next_pc;
    code.push(Instruction {
        pc,
        op,
        operands: operands.into(),
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_freezes_graph_before_linking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entry_path = dir.path().join("entry.ts");
        let dep_path = dir.path().join("dep.ts");
        std::fs::write(
            &entry_path,
            "import { value } from './dep.ts'; export const out = value;",
        )
        .expect("write entry");
        std::fs::write(&dep_path, "export const value = 7;").expect("write dep");

        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry_url = format!(
            "file://{}",
            std::fs::canonicalize(&entry_path).unwrap().display()
        );
        let entry_text = std::fs::read_to_string(&entry_path).unwrap();

        let graph = ModuleGraphBuilder::new(
            &loader,
            entry_url.clone(),
            SourceKind::TypeScript,
            entry_text,
        )
        .build()
        .expect("build graph");

        assert_eq!(graph.entry_url, entry_url);
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.nodes.contains_key(&entry_url));
        let order = topological_order(&graph.nodes, &entry_url).expect("topological order");
        assert_eq!(order.len(), 2);

        let linked = graph.link().expect("link graph");
        assert_eq!(linked.entry_url, entry_url);
        assert_eq!(linked.module.functions[0].name, "<entry>");
        assert_eq!(linked.module.module_inits.len(), 2);
        assert_eq!(linked.metadata.len(), 2);
    }
}
