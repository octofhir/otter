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
//! - Literal dynamic-import target failures are deferred into a synthetic
//!   module init so the eventual `import()` rejects instead of failing the
//!   entry graph.
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
use std::time::{Duration, Instant};

use otter_bytecode::{
    BytecodeModule, Constant, Function, FunctionCode, FunctionCodeBuilder, ModuleInit,
    ModuleResolution, Op, Operand, SourceKind as BytecodeSourceKind, SpanEntry,
};
use otter_compiler::{
    CompileError, CompiledExport, CompiledModuleMetadata, ModuleHostInfo, ResolvedBinding,
    compile_module_program_to_module,
};
use otter_syntax::{SourceKind, SyntaxError, with_program};
use oxc_ast::ast::{Expression, Program};
use oxc_ast_visit::Visit;

use crate::module_loader::{LoaderError, ModuleLoader};

/// Hard cap on graph traversal depth. Cycles and absurdly deep
/// trees both surface here as a catchable `RangeError`-shaped
/// diagnostic before exhausting the host stack.
pub const MODULE_DEPTH_LIMIT: usize = 256;

/// Opt-in phase timings for one module-graph load and execution.
///
/// Graph construction fills resolve, load, parse, compile, and link. The
/// runtime adds module instantiation to link time and records execute time.
/// Every value is cumulative across all modules in the graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModulePhaseTimings {
    /// Canonical specifier resolution time.
    pub resolve_time_ns: u64,
    /// Source read/fetch time.
    pub load_time_ns: u64,
    /// OXC parse time.
    pub parse_time_ns: u64,
    /// Bytecode lowering and CodeBlock construction time.
    pub compile_time_ns: u64,
    /// Graph validation, merge, and runtime instantiation time.
    pub link_time_ns: u64,
    /// Interpreter/JIT execution and microtask-drain time.
    pub execute_time_ns: u64,
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

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
    /// §16.2.1.6 ResolveExport failure: a named import or named
    /// re-export references a binding the target module does not
    /// unambiguously export. Surfaces as a resolution-phase
    /// `SyntaxError`.
    #[error("SyntaxError: {message}")]
    Resolution {
        /// URL of the module whose import/re-export failed to resolve.
        url: String,
        /// Human-readable description of the unresolved binding.
        message: String,
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
    /// `module_url → verbatim source text` for every real compiled
    /// module, forwarded to the interpreter so `Error.prototype.stack`
    /// and `util.getCallSites` can resolve frame spans to `(line, col)`.
    module_sources: BTreeMap<String, String>,
}

impl ModuleGraph {
    fn link(self) -> Result<LinkedProgram, GraphError> {
        validate_resolution(&self.nodes)?;
        let order = topological_order(&self.nodes, &self.entry_url)?;
        let module = link(&self.nodes, &order, &self.entry_url);
        let mut resolved = compute_resolved_exports(&self.nodes);
        let metadata = order
            .iter()
            .filter_map(|url| {
                self.nodes.get(url).map(|node| {
                    let mut metadata = node.metadata.clone();
                    if let Some(table) = resolved.remove(url) {
                        metadata.resolved_exports = table;
                    }
                    metadata
                })
            })
            .filter(|metadata| !metadata.source_url.is_empty())
            .collect();

        Ok(LinkedProgram {
            module,
            entry_url: self.entry_url,
            metadata,
            module_sources: self.module_sources,
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
    module_sources: BTreeMap<String, String>,
    timings: Option<ModulePhaseTimings>,
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
            module_sources: BTreeMap::new(),
            timings: None,
        }
    }

    fn new_profiled(
        loader: &'a ModuleLoader,
        entry_url: String,
        entry_kind: SourceKind,
        entry_text: String,
        timings: ModulePhaseTimings,
    ) -> Self {
        let mut builder = Self::new(loader, entry_url, entry_kind, entry_text);
        builder.timings = Some(timings);
        builder
    }

    fn add_resolve_time(&mut self, elapsed: Duration) {
        if let Some(timings) = &mut self.timings {
            timings.resolve_time_ns = timings.resolve_time_ns.saturating_add(duration_ns(elapsed));
        }
    }

    fn add_load_time(&mut self, elapsed: Duration) {
        if let Some(timings) = &mut self.timings {
            timings.load_time_ns = timings.load_time_ns.saturating_add(duration_ns(elapsed));
        }
    }

    fn add_parse_time(&mut self, elapsed: Duration) {
        if let Some(timings) = &mut self.timings {
            timings.parse_time_ns = timings.parse_time_ns.saturating_add(duration_ns(elapsed));
        }
    }

    fn add_compile_time(&mut self, elapsed: Duration) {
        if let Some(timings) = &mut self.timings {
            timings.compile_time_ns = timings.compile_time_ns.saturating_add(duration_ns(elapsed));
        }
    }

    fn resolve(&mut self, specifier: &str, referrer: Option<&str>) -> Result<String, LoaderError> {
        if self.timings.is_none() {
            return self.loader.resolve(specifier, referrer);
        }
        let started = Instant::now();
        let result = self.loader.resolve(specifier, referrer);
        self.add_resolve_time(started.elapsed());
        result
    }

    fn load_resolved(
        &mut self,
        url: String,
    ) -> Result<crate::module_loader::ResolvedSource, LoaderError> {
        if self.timings.is_none() {
            return self.loader.load_resolved(url);
        }
        let started = Instant::now();
        let result = self.loader.load_resolved(url);
        self.add_load_time(started.elapsed());
        result
    }

    fn read_text_file(&mut self, url: &str) -> Result<String, LoaderError> {
        let path = url.strip_prefix("file://").unwrap_or(url);
        if self.timings.is_none() {
            return std::fs::read_to_string(path).map_err(|error| LoaderError::Load {
                url: url.to_string(),
                message: error.to_string(),
            });
        }
        let started = Instant::now();
        let result = std::fs::read_to_string(path).map_err(|error| LoaderError::Load {
            url: url.to_string(),
            message: error.to_string(),
        });
        self.add_load_time(started.elapsed());
        result
    }

    fn build_with_timings(
        mut self,
    ) -> Result<(ModuleGraph, Option<ModulePhaseTimings>), GraphError> {
        while let Some((url, kind, text, dynamic)) = self.queue.pop() {
            let url_for_error = url.clone();
            if let Err(err) = self.load_one(url, kind, text, dynamic) {
                if dynamic {
                    self.insert_dynamic_failure_node(url_for_error, &err)?;
                    continue;
                }
                return Err(err);
            }
        }
        Ok((
            ModuleGraph {
                entry_url: self.entry_url,
                nodes: self.nodes,
                module_sources: self.module_sources,
            },
            self.timings,
        ))
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

        // Retain the verbatim source so the interpreter can map this
        // module's frame spans back to `(line, column)` for
        // `Error.prototype.stack` / `util.getCallSites`. Captured before
        // `with_program` consumes `text`.
        self.module_sources.insert(url.clone(), text.clone());

        let timing_enabled = self.timings.is_some();
        let compile_program = |program: &Program<'_>| {
            let requests = collect_module_requests(program);
            let mut resolved_imports: HashMap<String, String> = HashMap::new();
            let mut deps: Vec<ModuleEdge> = Vec::with_capacity(requests.len());
            let mut queued: Vec<(String, SourceKind, String, bool)> = Vec::new();
            let mut eager_static_specs: HashSet<String> = HashSet::new();
            let mut dynamic_specs: HashSet<String> = HashSet::new();
            for request in &requests {
                // import-attributes `type: "text"` — the attribute is
                // part of the module-map key, so the text variant gets
                // its own marker URL and a synthesised
                // `export default "<raw>"` module node. Resolution
                // accepts extension-less fixture paths the normal
                // probing resolver would reject.
                if request.attr_type.as_deref() == Some("text") {
                    let base = match self.resolve(&request.specifier, Some(&url)) {
                        Ok(target) => target,
                        Err(_) => resolve_plain_relative_file(&request.specifier, &url)
                            .ok_or_else(|| {
                                GraphError::Loader(LoaderError::Resolve {
                                    specifier: request.specifier.clone(),
                                    referrer: url.clone(),
                                    message: "text module fixture not found".to_string(),
                                })
                            })?,
                    };
                    let target = format!("{base}{TEXT_MODULE_MARKER}");
                    resolved_imports.insert(request.specifier.clone(), target.clone());
                    if !request.deferred && !request.dynamic {
                        eager_static_specs.insert(request.specifier.clone());
                    }
                    deps.push(ModuleEdge {
                        target: target.clone(),
                        deferred: request.deferred,
                    });
                    if !self.nodes.contains_key(&target) {
                        let raw = self.read_text_file(&base)?;
                        let escaped = serde_json::to_string(&raw).unwrap_or_default();
                        let shim = format!("export default ({escaped});\n");
                        queued.push((
                            target,
                            SourceKind::JavaScript,
                            shim,
                            optional_dynamic || request.dynamic,
                        ));
                    }
                    continue;
                }
                let target = self.resolve(&request.specifier, Some(&url))?;
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
                    let loaded = self.load_resolved(target)?;
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
            let compile_started = timing_enabled.then(Instant::now);
            let compiled = compile_module_program_to_module(program, kind, &host);
            if let Some(started) = compile_started {
                self.add_compile_time(started.elapsed());
            }
            let mut compiled = compiled.map_err(|e| GraphError::Compile {
                url: url.clone(),
                error: e,
            })?;
            for edge in &mut compiled.bytecode.module_resolutions {
                if dynamic_specs.contains(&edge.specifier)
                    && !eager_static_specs.contains(&edge.specifier)
                {
                    edge.deferred = true;
                    // Distinguish `import("x")` preloads from
                    // `import defer`: the entry driver must not
                    // force-evaluate an async dynamic target eagerly
                    // — `import()` settles through its own promise.
                    edge.dynamic = true;
                }
            }
            // §16.2.1.5 InnerModuleEvaluation walks `[[RequestedModules]]`
            // in source order. The compiler assembles `module_resolutions`
            // from a hash map, losing that order; restore it from the
            // source-ordered request list so eager evaluation (which roots
            // its DFS at the entry and recurses dependencies in this order)
            // matches the spec's deterministic evaluation order.
            let request_order: HashMap<&str, usize> = requests
                .iter()
                .enumerate()
                .map(|(idx, request)| (request.specifier.as_str(), idx))
                .collect();
            compiled.bytecode.module_resolutions.sort_by_key(|edge| {
                request_order
                    .get(edge.specifier.as_str())
                    .copied()
                    .unwrap_or(usize::MAX)
            });
            Ok::<_, GraphError>((compiled, deps, queued))
        };
        let parsed = if timing_enabled {
            let (parsed, parse_time) =
                otter_syntax::with_program_timing(text, kind, compile_program).map_err(
                    |error| GraphError::Parse {
                        url: url.clone(),
                        error,
                    },
                )?;
            self.add_parse_time(parse_time);
            parsed
        } else {
            with_program(text, kind, compile_program).map_err(|error| GraphError::Parse {
                url: url.clone(),
                error,
            })?
        };
        let (compiled, deps, queued) = parsed?;

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

    fn insert_dynamic_failure_node(
        &mut self,
        url: String,
        err: &GraphError,
    ) -> Result<(), GraphError> {
        if self.nodes.contains_key(&url) {
            return Ok(());
        }
        let fragment = dynamic_failure_fragment(&url, err)?;
        self.nodes.insert(
            url,
            ModuleNode {
                fragment,
                metadata: CompiledModuleMetadata::default(),
                deps: Vec::new(),
            },
        );
        Ok(())
    }
}

fn dynamic_failure_fragment(url: &str, err: &GraphError) -> Result<BytecodeModule, GraphError> {
    let ctor = dynamic_failure_constructor(err);
    let message = format!("dynamic import: load failed for \"{url}\": {err:?}");
    let message_literal =
        serde_json::to_string(&message).unwrap_or_else(|_| "\"dynamic import failed\"".to_string());
    let source = format!("throw new {ctor}({message_literal});");
    with_program(source, SourceKind::JavaScript, |program| {
        let host = ModuleHostInfo {
            module_url: url.to_string(),
            resolved_imports: HashMap::new(),
        };
        compile_module_program_to_module(program, SourceKind::JavaScript, &host).map_err(|error| {
            GraphError::Compile {
                url: url.to_string(),
                error,
            }
        })
    })
    .map_err(|error| GraphError::Parse {
        url: url.to_string(),
        error,
    })?
    .map(|compiled| compiled.bytecode)
}

fn dynamic_failure_constructor(err: &GraphError) -> &'static str {
    match err {
        GraphError::Parse { .. } => "SyntaxError",
        GraphError::Compile { error, .. } => match error {
            CompileError::Syntax { .. } => "SyntaxError",
            _ => "TypeError",
        },
        GraphError::Cycle { .. } => "RangeError",
        GraphError::Resolution { .. } => "SyntaxError",
        GraphError::Loader(_) => "TypeError",
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

/// Module-map key suffix distinguishing the `with { type: "text" }`
/// variant of a URL from its JavaScript-module variant. Stripped
/// nowhere — the synthesised text node carries its full shim source,
/// so the marker URL is never read from disk.
const TEXT_MODULE_MARKER: &str = "#otter-module-type=text";

/// Join a relative specifier against the referrer's directory and
/// canonicalise, without extension probing — text-module fixtures
/// commonly have no extension at all.
fn resolve_plain_relative_file(specifier: &str, referrer_url: &str) -> Option<String> {
    let referrer_path = referrer_url.strip_prefix("file://")?;
    let dir = Path::new(referrer_path).parent()?;
    let joined = dir.join(specifier);
    let canonical = std::fs::canonicalize(&joined).ok()?;
    Some(format!("file://{}", canonical.display()))
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
    /// `type` import attribute from the `with { type: "…" }` clause,
    /// when present. Part of the module-map key per the
    /// import-attributes proposal.
    attr_type: Option<String>,
}

impl ModuleRequestVisitor {
    fn record(&mut self, specifier: &str, deferred: bool, dynamic: bool) {
        self.record_with_type(specifier, deferred, dynamic, None);
    }

    fn record_with_type(
        &mut self,
        specifier: &str,
        deferred: bool,
        dynamic: bool,
        attr_type: Option<String>,
    ) {
        let key = (format!("{specifier}\x00{attr_type:?}"), deferred);
        if self.seen.insert(key) {
            self.out.push(ModuleRequest {
                specifier: specifier.to_string(),
                deferred,
                dynamic,
                attr_type,
            });
        }
    }
}

/// Extract the `type` attribute value from a `with { … }` clause.
fn with_clause_type(clause: Option<&oxc_ast::ast::WithClause<'_>>) -> Option<String> {
    let clause = clause?;
    clause.with_entries.iter().find_map(|entry| {
        let key = match &entry.key {
            oxc_ast::ast::ImportAttributeKey::Identifier(id) => id.name.as_str(),
            oxc_ast::ast::ImportAttributeKey::StringLiteral(lit) => lit.value.as_str(),
        };
        (key == "type").then(|| entry.value.value.as_str().to_string())
    })
}

impl<'a> Visit<'a> for ModuleRequestVisitor {
    fn visit_import_declaration(&mut self, decl: &oxc_ast::ast::ImportDeclaration<'a>) {
        if !decl.import_kind.is_type() {
            self.record_with_type(
                decl.source.value.as_str(),
                matches!(decl.phase, Some(oxc_ast::ast::ImportPhase::Defer)),
                false,
                with_clause_type(decl.with_clause.as_deref()),
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
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::ReturnUndefined, &[]);
    BytecodeModule {
        module: url.to_string(),
        template_sites: Vec::new(),
        source_kind: BytecodeSourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<hosted-module-init>".to_string(),
            param_count: 2,
            is_module: true,
            module_url: url.to_string(),
            code: code.finish(),
            spans: Vec::new(),
            ..Default::default()
        }],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// §16.2.1.6 ResolveExport result for one binding lookup.
#[derive(Clone, PartialEq, Eq)]
enum Resolution {
    /// No export by this name is reachable.
    Null,
    /// Distinct star re-exports resolve the name to different bindings.
    Ambiguous,
    /// Resolved to a concrete `(module_url, binding_name)`.
    Resolved { module: String, binding: String },
}

/// Whether `url` names a real parsed source module whose export
/// surface we model. Host/builtin fragments carry an empty
/// `source_url` and are treated as always-resolvable so we never
/// reject `node:`/hosted imports we do not analyse.
fn is_resolvable_module(nodes: &BTreeMap<String, ModuleNode>, url: &str) -> bool {
    nodes
        .get(url)
        .is_some_and(|node| !node.metadata.source_url.is_empty())
}

/// Resolve `specifier` against `from_url`'s recorded import edges to
/// the canonical target URL, if statically known.
fn resolve_specifier<'a>(
    nodes: &'a BTreeMap<String, ModuleNode>,
    from_url: &str,
    specifier: &str,
) -> Option<&'a str> {
    let meta = &nodes.get(from_url)?.metadata;
    meta.imports
        .iter()
        .find(|import| import.specifier == specifier)
        .and_then(|import| import.target.as_deref())
}

/// §16.2.1.6 ResolveExport(module, exportName, resolveSet). `path`
/// is the active `(module, name)` lookup chain used to short-circuit
/// import cycles (returns [`Resolution::Null`] on revisit).
fn resolve_export(
    nodes: &BTreeMap<String, ModuleNode>,
    url: &str,
    name: &str,
    path: &mut Vec<(String, String)>,
) -> Resolution {
    // Unmodelled (host/builtin) target: any imported name is assumed
    // to resolve.
    if !is_resolvable_module(nodes, url) {
        return Resolution::Resolved {
            module: url.to_string(),
            binding: name.to_string(),
        };
    }
    let key = (url.to_string(), name.to_string());
    if path.iter().any(|entry| entry == &key) {
        return Resolution::Null;
    }
    path.push(key);

    let exports: &[CompiledExport] = &nodes.get(url).expect("checked above").metadata.exports;
    let mut result = Resolution::Null;

    // 1. Direct export entries (local, default, named re-export, or
    //    `export * as ns`) matching `name`.
    let mut found_direct = false;
    for export in exports {
        if export.name != name {
            continue;
        }
        match (export.local.as_deref(), export.from.as_deref()) {
            // `export { local as name } from "from"` — indirect named
            // re-export; resolve through the source module.
            (Some(local), Some(from)) => {
                result = match resolve_specifier(nodes, url, from) {
                    Some(target) => resolve_export(nodes, target, local, path),
                    None => Resolution::Null,
                };
            }
            // `export * as name from "from"` — namespace re-export.
            // §15.2.1.16.3 step 6.a.iii resolves to the *source*
            // module's namespace binding, so two modules re-exporting
            // the same namespace match (unambiguous).
            (None, Some(from)) => {
                result = match resolve_specifier(nodes, url, from) {
                    Some(target) => Resolution::Resolved {
                        module: target.to_string(),
                        binding: "*namespace*".to_string(),
                    },
                    None => Resolution::Null,
                };
            }
            // `export { local }` / `export default …`. If `local` is
            // itself an imported binding, the export is indirect and
            // resolves through that import (§16.2.1.7.1 step 10.1.ii);
            // otherwise it is a terminal local binding.
            _ => {
                let local_name = export.local.as_deref().unwrap_or(name);
                let via_import = nodes
                    .get(url)
                    .and_then(|node| {
                        node.metadata
                            .named_imports
                            .iter()
                            .find(|imp| imp.local == local_name)
                    })
                    .cloned();
                result = match via_import {
                    Some(imp) if imp.is_namespace => {
                        match resolve_specifier(nodes, url, &imp.specifier) {
                            Some(target) => Resolution::Resolved {
                                module: target.to_string(),
                                // §16.2.1.6: re-exporting an `import defer * as`
                                // binding preserves deferred namespace semantics.
                                binding: if imp.is_deferred {
                                    "*deferred-namespace*".to_string()
                                } else {
                                    "*namespace*".to_string()
                                },
                            },
                            None => Resolution::Null,
                        }
                    }
                    Some(imp) => match resolve_specifier(nodes, url, &imp.specifier) {
                        Some(target) => resolve_export(nodes, target, &imp.name, path),
                        None => Resolution::Null,
                    },
                    // Terminal local binding. The defining module holds
                    // it live on its environment under the *exported*
                    // name (`name`), not the local — `export { local as
                    // name }` mirrors writes to `local` onto env[name].
                    // Returning `name` keeps the resolution aligned with
                    // the env key the runtime actually reads.
                    None => Resolution::Resolved {
                        module: url.to_string(),
                        binding: name.to_string(),
                    },
                };
            }
        }
        found_direct = true;
        break;
    }

    // 2. Star re-exports (`export * from "m"`), excluding `default`.
    if !found_direct && name != "default" {
        let mut star = Resolution::Null;
        for export in exports {
            if export.name != "*" {
                continue;
            }
            let Some(from) = export.from.as_deref() else {
                continue;
            };
            let Some(target) = resolve_specifier(nodes, url, from) else {
                continue;
            };
            match resolve_export(nodes, target, name, path) {
                Resolution::Ambiguous => {
                    star = Resolution::Ambiguous;
                    break;
                }
                candidate @ Resolution::Resolved { .. } => match &star {
                    Resolution::Null => star = candidate,
                    Resolution::Resolved { .. } => {
                        if star != candidate {
                            star = Resolution::Ambiguous;
                            break;
                        }
                    }
                    Resolution::Ambiguous => {}
                },
                Resolution::Null => {}
            }
        }
        result = star;
    }

    path.pop();
    result
}

/// Validate every named import and named re-export across the graph
/// (§16.2.1.6). A name that resolves to nothing or ambiguously is a
/// resolution-phase `SyntaxError`.
fn validate_resolution(nodes: &BTreeMap<String, ModuleNode>) -> Result<(), GraphError> {
    for (url, node) in nodes {
        if node.metadata.source_url.is_empty() {
            continue;
        }
        for import in &node.metadata.named_imports {
            // Namespace imports (`import * as ns`) bind the module
            // namespace object directly and need no export lookup.
            if import.is_namespace {
                continue;
            }
            check_binding(nodes, url, &import.specifier, &import.name, "import")?;
        }
        for export in &node.metadata.exports {
            // Only named re-exports (`export { local as name } from`)
            // require a binding lookup; star and namespace re-exports
            // do not.
            if let (Some(local), Some(from)) = (export.local.as_deref(), export.from.as_deref()) {
                check_binding(nodes, url, from, local, "re-export")?;
            }
        }
    }
    Ok(())
}

/// Resolve `name` against the module reached through `specifier` from
/// `url`; error if the binding is missing or ambiguous.
fn check_binding(
    nodes: &BTreeMap<String, ModuleNode>,
    url: &str,
    specifier: &str,
    name: &str,
    kind: &str,
) -> Result<(), GraphError> {
    let Some(target) = resolve_specifier(nodes, url, specifier) else {
        return Ok(());
    };
    if !is_resolvable_module(nodes, target) {
        return Ok(());
    }
    let mut path = Vec::new();
    match resolve_export(nodes, target, name, &mut path) {
        Resolution::Resolved { .. } => Ok(()),
        _ => Err(GraphError::Resolution {
            url: url.to_string(),
            message: format!(
                "{kind} of `{name}` from `{specifier}` does not resolve to an exported binding"
            ),
        }),
    }
}

/// Append `name` to `names` if not already present.
fn push_unique(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|existing| existing == name) {
        names.push(name.to_string());
    }
}

/// §16.2.1.7 GetExportedNames(module, exportStarSet) — the set of names
/// a module's namespace can expose. Local and indirect export entries
/// (named exports, named re-exports, `export * as ns`) contribute their
/// exported name; bare `export *` entries contribute every non-`default`
/// name of the starred module, recursively. `default` is included for a
/// module's own entries (it is importable directly) but is never pulled
/// in through a star. `seen` is the `exportStarSet` that breaks
/// `export *` cycles.
fn module_exported_names(
    nodes: &BTreeMap<String, ModuleNode>,
    url: &str,
    seen: &mut Vec<String>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if seen.iter().any(|visited| visited == url) {
        return names;
    }
    seen.push(url.to_string());
    let Some(node) = nodes.get(url) else {
        return names;
    };
    for export in &node.metadata.exports {
        if export.name == "*" {
            let Some(from) = export.from.as_deref() else {
                continue;
            };
            let Some(target) = resolve_specifier(nodes, url, from) else {
                continue;
            };
            for name in module_exported_names(nodes, target, seen) {
                if name != "default" {
                    push_unique(&mut names, &name);
                }
            }
        } else {
            push_unique(&mut names, &export.name);
        }
    }
    names
}

/// Build every modeled module's §16.2.1.6 ResolveExport table:
/// exported name → resolved live binding. Names that resolve to nothing
/// or ambiguously are omitted so the namespace never exposes them
/// (§10.4.6 / §16.2.1.10 unambiguousNames). Computed once at link time
/// and threaded to the runtime through each module's
/// [`CompiledModuleMetadata::resolved_exports`].
fn compute_resolved_exports(
    nodes: &BTreeMap<String, ModuleNode>,
) -> HashMap<String, BTreeMap<String, ResolvedBinding>> {
    let mut out: HashMap<String, BTreeMap<String, ResolvedBinding>> = HashMap::new();
    for (url, node) in nodes {
        if node.metadata.source_url.is_empty() {
            continue;
        }
        let mut table: BTreeMap<String, ResolvedBinding> = BTreeMap::new();
        for name in module_exported_names(nodes, url, &mut Vec::new()) {
            let mut path = Vec::new();
            if let Resolution::Resolved { module, binding } =
                resolve_export(nodes, url, &name, &mut path)
            {
                table.insert(
                    name,
                    ResolvedBinding {
                        defining_module: module,
                        binding,
                    },
                );
            }
        }
        out.insert(url.clone(), table);
    }
    out
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
    /// `module_url → verbatim source text` for every real compiled
    /// module. The runtime registers these with the interpreter before
    /// evaluation so frame spans resolve to `(line, column)`.
    pub module_sources: BTreeMap<String, String>,
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
    load_program_inner(loader, entry_path, None).map(|(linked, _)| linked)
}

/// Load, compile, and link a module graph while recording phase timings.
///
/// This is an explicit benchmark/diagnostic path. [`load_program`] performs the
/// same work without clock reads or timing accumulation.
///
/// # Errors
/// See [`GraphError`].
pub fn load_program_profiled(
    loader: &ModuleLoader,
    entry_path: &Path,
) -> Result<(LinkedProgram, ModulePhaseTimings), GraphError> {
    let (linked, timings) =
        load_program_inner(loader, entry_path, Some(ModulePhaseTimings::default()))?;
    Ok((linked, timings.expect("profiled graph carries timings")))
}

fn load_program_inner(
    loader: &ModuleLoader,
    entry_path: &Path,
    mut timings: Option<ModulePhaseTimings>,
) -> Result<(LinkedProgram, Option<ModulePhaseTimings>), GraphError> {
    // Read the entry directly so the user sees clear errors when
    // the entry path is malformed before any specifier-resolution
    // logic runs.
    let resolve_started = timings.is_some().then(Instant::now);
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
    if let (Some(timings), Some(started)) = (&mut timings, resolve_started) {
        timings.resolve_time_ns = duration_ns(started.elapsed());
    }
    let load_started = timings.is_some().then(Instant::now);
    let entry_text = std::fs::read_to_string(entry_path).map_err(|e| LoaderError::Load {
        url: entry_url.clone(),
        message: e.to_string(),
    })?;
    if let (Some(timings), Some(started)) = (&mut timings, load_started) {
        timings.load_time_ns = duration_ns(started.elapsed());
    }

    let builder = match timings {
        Some(timings) => {
            ModuleGraphBuilder::new_profiled(loader, entry_url, entry_kind, entry_text, timings)
        }
        None => ModuleGraphBuilder::new(loader, entry_url, entry_kind, entry_text),
    };
    let (graph, mut timings) = builder.build_with_timings()?;
    let link_started = timings.is_some().then(Instant::now);
    let linked = graph.link()?;
    if let (Some(timings), Some(started)) = (&mut timings, link_started) {
        timings.link_time_ns = duration_ns(started.elapsed());
    }
    Ok((linked, timings))
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

    // Synthesise <entry>'s body: one `Op::EvaluateModule` per
    // evaluation root, awaited when the graph evaluates async.
    let entry_body = build_entry_body(nodes, order, entry_url, &mut constants);
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
        template_sites: Vec::new(),
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
fn rewrite_const_indices(code: &FunctionCode, offset: u32) -> FunctionCode {
    let mut rewritten = FunctionCodeBuilder::new();
    for instr in code {
        let operands = code
            .operands(instr)
            .iter()
            .enumerate()
            .map(|(pos, operand)| match operand {
                Operand::ConstIndex(k) if instr.op.is_const_pool_operand(pos) => {
                    Operand::ConstIndex(k + offset)
                }
                other => other,
            })
            .collect::<Vec<_>>();
        rewritten.push(instr.op, &operands);
    }
    rewritten.finish()
}

/// One assembled `<entry>` body.
struct EntryBody {
    code: FunctionCode,
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
    constants: &mut Vec<Constant>,
) -> EntryBody {
    let mut code = FunctionCodeBuilder::new();
    let mut spans: Vec<SpanEntry> = Vec::new();

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
            } else if !edge.dynamic {
                // `import defer` only: an async deferred target cannot
                // be force-evaluated synchronously, so the proposal
                // evaluates it eagerly. `import("x")` preloads stay
                // lazy — the import-call evaluates its target and
                // settles through the returned promise (§13.3.10).
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

    // §16.2.1.5 Evaluate → InnerModuleEvaluation. The DFS is rooted at
    // the entry: `Op::EvaluateModule` recurses into each module's
    // dependencies before running its own body, so evaluating the
    // entry alone visits the whole eagerly-reachable graph in
    // post-order. Rooting at the entry is what makes cyclic graphs
    // correct — a back-edge (`export … from` that points back at an
    // ancestor) finds the ancestor already on the evaluating stack and
    // is skipped, instead of running the ancestor's body first.
    //
    // An async-evaluated `import defer` target cannot be
    // force-evaluated synchronously on first namespace access, so the
    // proposal evaluates it eagerly: its TLA roots run as additional
    // evaluation roots *before* the entry, and their gates are awaited
    // so the entry body observes settled deferred namespaces.
    //
    // Each `Op::EvaluateModule` writes the module's evaluation gate
    // promise (or `undefined` for a synchronously completed subtree)
    // to its register; an async graph compiles the `<entry>` as an
    // async function that awaits each gate. A top-level-await module
    // parks only its own gate — InnerModuleEvaluation lets siblings
    // keep evaluating while it is suspended.
    let mut next_reg: u16 = 0;
    let mut gate_regs: Vec<u16> = Vec::new();
    let emit_evaluate = |code: &mut FunctionCodeBuilder,
                         spans: &mut Vec<SpanEntry>,
                         constants: &mut Vec<Constant>,
                         next_reg: &mut u16,
                         url: &str|
     -> u16 {
        let url_const_idx = intern_string_const(constants, url);
        let dst = *next_reg;
        *next_reg += 1;
        emit_op(
            code,
            spans,
            Op::EvaluateModule,
            [Operand::Register(dst), Operand::ConstIndex(url_const_idx)],
        );
        dst
    };
    let emit_awaits = |code: &mut FunctionCodeBuilder,
                       spans: &mut Vec<SpanEntry>,
                       next_reg: &mut u16,
                       gates: &mut Vec<u16>| {
        for gate in gates.drain(..) {
            let r_awaited = *next_reg;
            *next_reg += 1;
            emit_op(
                code,
                spans,
                Op::Await,
                [Operand::Register(r_awaited), Operand::Register(gate)],
            );
        }
    };
    // Deferred-async TLA roots need no special handling here:
    // InnerModuleEvaluation gathers them into the importing module's
    // own evaluation list (import-defer proposal), in request order.
    let entry_gate = emit_evaluate(&mut code, &mut spans, constants, &mut next_reg, entry_url);
    gate_regs.push(entry_gate);
    // Idempotent no-op sweeps for any module reachable outside the
    // entry's own DFS (defensive parity with the records' walk).
    for url in order {
        if url == entry_url || !reachable.contains(url) {
            continue;
        }
        let dst = emit_evaluate(&mut code, &mut spans, constants, &mut next_reg, url);
        gate_regs.push(dst);
    }
    if has_async {
        emit_awaits(&mut code, &mut spans, &mut next_reg, &mut gate_regs);
    }
    emit_op(&mut code, &mut spans, Op::ReturnUndefined, []);
    EntryBody {
        code: code.finish(),
        spans,
        scratch: next_reg,
        is_async: has_async,
    }
}

fn emit_op(
    code: &mut FunctionCodeBuilder,
    spans: &mut Vec<SpanEntry>,
    op: Op,
    operands: impl AsRef<[Operand]>,
) {
    let pc = code.next_pc();
    code.push(op, operands.as_ref());
    spans.push(SpanEntry { pc, span: (0, 0) });
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

        let (graph, timings) = ModuleGraphBuilder::new(
            &loader,
            entry_url.clone(),
            SourceKind::TypeScript,
            entry_text,
        )
        .build_with_timings()
        .expect("build graph");

        assert_eq!(timings, None);
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

    #[test]
    fn profiled_load_reports_all_graph_phases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entry_path = dir.path().join("entry.mjs");
        let dep_path = dir.path().join("dep.mjs");
        std::fs::write(
            &entry_path,
            "import { value } from './dep.mjs'; export const out = value;",
        )
        .expect("write entry");
        std::fs::write(&dep_path, "export const value = 7;").expect("write dep");

        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let (linked, timings) = load_program_profiled(&loader, &entry_path).expect("profile graph");

        assert_eq!(linked.module.module_inits.len(), 2);
        assert!(timings.resolve_time_ns > 0);
        assert!(timings.load_time_ns > 0);
        assert!(timings.parse_time_ns > 0);
        assert!(timings.compile_time_ns > 0);
        assert!(timings.link_time_ns > 0);
        assert_eq!(timings.execute_time_ns, 0);
    }
}
