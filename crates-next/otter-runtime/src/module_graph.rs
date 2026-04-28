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
//! - [task 36a](../../docs/new-engine/tasks/36a-modules-graph-and-live-bindings.md)
//! - <https://tc39.es/ecma262/#sec-cyclic-module-records>
//!   — spec model for the cyclic-graph evaluation algorithm we
//!   approximate with post-order DFS + literal `import()`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use otter_bytecode::{
    BytecodeModule, Constant, Function, Instruction, ModuleInit, ModuleResolution, Op, Operand,
    SourceKind as BytecodeSourceKind, SpanEntry,
};
use otter_compiler::{ModuleHostInfo, compile_module_fragment};
use otter_syntax::{Parsed, SourceKind, parse};
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
    #[error("parse failed for `{url}`: {message}")]
    Parse {
        /// Module URL.
        url: String,
        /// Joined OXC diagnostic messages.
        message: String,
    },
    /// Compiler rejected the module fragment.
    #[error("compile failed for `{url}`: {message}")]
    Compile {
        /// Module URL.
        url: String,
        /// Compiler diagnostic message.
        message: String,
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
fn collect_specifiers(parsed: &Parsed) -> Result<Vec<String>, GraphError> {
    let program = parsed.program().map_err(|e| GraphError::Parse {
        url: "<unknown>".to_string(),
        message: e.messages.join("; "),
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
        load_count += 1;
        if load_count > MODULE_DEPTH_LIMIT {
            return Err(GraphError::Cycle { url });
        }
        let parsed = parse(text, kind).map_err(|e| GraphError::Parse {
            url: url.clone(),
            message: e.messages.join("; "),
        })?;
        let specifiers = collect_specifiers(&parsed)?;
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
        let fragment =
            compile_module_fragment(&parsed, &host).map_err(|e| GraphError::Compile {
                url: url.clone(),
                message: format!("{e:?}"),
            })?;
        let _ = url; // url is the BTreeMap key; ModuleNode itself doesn't need it
        nodes.insert(nodes_key_for(&fragment), ModuleNode { fragment, deps });
    }
    Ok(nodes)
}

/// Topological sort of `nodes`, post-order DFS rooted at `entry`.
/// Cycles raise [`GraphError::Cycle`].
///
/// Iterative two-pass DFS to avoid recursion-depth concerns in
/// the host: each visit-frame on the work stack is `(url,
/// children_iterator_position)`. When we finish the children we
/// emit the URL into `order`.
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
            Some(Mark::Done) => continue,
            Some(Mark::InProgress) => {
                return Err(GraphError::Cycle {
                    url: target.clone(),
                });
            }
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

    Ok(LinkedProgram { module, entry_url })
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
/// `Operand::ConstIndex` operand that **actually indexes the
/// constant pool** — `Operand::ConstIndex` is also reused to
/// carry raw counts (`argc`, `upvalue_count`, `array_length`)
/// for some opcodes; those slots must NOT be offset.
///
/// The per-opcode mapping of which operand positions carry
/// constant-pool refs vs. counts is encoded in
/// [`is_const_pool_ref`].
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
                        Operand::ConstIndex(k) if is_const_pool_ref(op, pos) => {
                            Operand::ConstIndex(k + offset)
                        }
                        other => other.clone(),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// `true` when operand position `pos` of `op` carries a
/// constant-pool reference (i.e., should be offset by the linker
/// during fragment merging). `false` for raw-count uses of
/// `Operand::ConstIndex` (`argc`, `upvalue_count`, etc.).
///
/// Spec mapping: every opcode that emits a `Constant::String`,
/// `Constant::Number`, `Constant::BigInt`, `Constant::FunctionId`,
/// `Constant::RegExp` reference goes here. Variadic call shapes
/// (`Op::Call`, `Op::CallWithThis`, `Op::New`, …) carry their
/// argc as a count, not an index, so those positions stay
/// unchanged.
fn is_const_pool_ref(op: Op, pos: usize) -> bool {
    match op {
        // [reg, const] shape
        Op::LoadString
        | Op::LoadNumber
        | Op::LoadBigInt
        | Op::LoadRegExp
        | Op::MakeFunction
        | Op::MathLoad
        | Op::ImportNamespace => pos == 1,
        // [reg, reg, const] shape
        Op::LoadProperty | Op::DeleteProperty => pos == 2,
        // [reg, const, reg] shape
        Op::StoreProperty => pos == 1,
        // [reg, function_const, count, parent_idxs...] —
        // function_const at pos 1; count at pos 2 stays raw.
        Op::MakeClosure => pos == 1,
        // [reg, name_const, argc, args...] — name at pos 1;
        // argc at pos 2 stays raw.
        Op::MathCall | Op::JsonCall | Op::PromiseCall => pos == 1,
        // [reg, recv, name_const, argc, args...] — name at pos 2;
        // argc at pos 3 stays raw.
        Op::CallMethodValue => pos == 2,
        // No constant-pool refs in any other operand position.
        _ => false,
    }
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
        emit_op(
            &mut code,
            &mut spans,
            &mut next_pc,
            Op::StoreProperty,
            vec![
                Operand::Register(r_meta),
                Operand::ConstIndex(url_name_idx),
                Operand::Register(r_url_str),
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
