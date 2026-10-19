//! ES-module lowering state and host-provided resolution metadata.
//!
//! # Contents
//! - import binding records
//! - module state maps
//! - module builder storage
//! - constant-pool interning ([`ModuleBuilder::intern_constant`])
//! - module source-kind helpers
//!
//! # Invariants
//! - Host-provided resolved imports are trusted by lowering.
//! - The constant pool is append-only. Interning returns the lowest index whose
//!   entry equals the candidate, exactly as a scan of the pool would; the
//!   content index only shortens the search.
//!
//! # See also
//! - `entry` for module compilation

use crate::*;

/// What a module asks its host for: the raw specifier text plus the
/// `type` import attribute that decides how the target is read. Both
/// halves are the key — `import a from "./x" with { type: "xml" }` and
/// `import b from "./x"` in one module are two requests resolving to
/// two targets, so nothing downstream may key on the specifier alone.
///
/// <https://tc39.es/proposal-import-attributes/>
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImportRequest {
    /// Raw specifier text from the declaration (`"./other.ts"`).
    pub specifier: String,
    /// `type` attribute of the `with { … }` clause, when present.
    pub attr_type: Option<String>,
}

impl ImportRequest {
    /// A request carrying a `type` attribute.
    #[must_use]
    pub fn new(specifier: impl Into<String>, attr_type: Option<String>) -> Self {
        Self {
            specifier: specifier.into(),
            attr_type,
        }
    }

    /// A request with no import attributes — the ordinary case.
    #[must_use]
    pub fn plain(specifier: impl Into<String>) -> Self {
        Self::new(specifier, None)
    }
}

/// The `type` attribute of a `with { … }` clause, when it has one.
pub(crate) fn import_attribute_type(
    clause: Option<&oxc_ast::ast::WithClause<'_>>,
) -> Option<String> {
    clause?.with_entries.iter().find_map(|entry| {
        let key = match &entry.key {
            oxc_ast::ast::ImportAttributeKey::Identifier(id) => id.name.as_str(),
            oxc_ast::ast::ImportAttributeKey::StringLiteral(lit) => lit.value.as_str(),
        };
        (key == "type").then(|| entry.value.value.as_str().to_string())
    })
}

/// One pre-resolved import-record binding: maps an importer-side
/// alias (`import { a as alias } from "./other.ts"`) to the
/// import record plus the original source-side name the property
/// load reads.
#[derive(Debug, Clone)]
pub(crate) struct ImportBinding {
    /// Import record number; its object lives in the module-scope slot
    /// named by [`crate::compiler::import_record_binding`].
    pub(crate) record: u16,
    /// Source-module name of the binding (e.g., the `a` in
    /// `import { a as alias } from "./other.ts"`). For default
    /// imports this is `"default"`. For namespace imports the
    /// alias resolves directly to the record itself; we store an
    /// empty string here as the sentinel.
    pub(crate) source_name: String,
    /// `true` for `import * as ns from "./..."` — the alias binds
    /// to the Module Namespace Exotic Object, resolved from
    /// `request` at read time (distinct from the raw env record).
    pub(crate) is_namespace: bool,
    /// The import's request (`"./other.ts"` plus any `type`
    /// attribute), used to resolve the target module this alias reads
    /// from.
    pub(crate) request: ImportRequest,
    /// `true` for `import defer * as ns` — the alias binds to the
    /// *deferred* namespace slot (lazy evaluation) rather than the
    /// eager Module Namespace Exotic Object.
    pub(crate) is_deferred: bool,
}

/// Module-mode state attached to a [`FunctionContext`] when the
/// function is the top-level `<module-init>` of an ES-module
/// fragment.
#[derive(Debug, Default)]
pub(crate) struct ModuleState {
    /// Per-request import record number. Populated by the import
    /// pre-pass; the record object lives in a module-scope slot.
    pub(crate) import_records: HashMap<ImportRequest, u16>,
    /// Importer-side alias → import-record binding info.
    pub(crate) imported_names: HashMap<String, ImportBinding>,
    /// Names that this module exports. Every assignment to a name
    /// in this set emits an extra
    /// `StoreProperty module_env, name, value` after the regular
    /// store so live-binding writes propagate.
    pub(crate) exported_names: HashSet<String>,
    /// Local binding name → exported names it backs via a renamed local
    /// re-export (`export { local as exported }`, no `from`). Every
    /// assignment to `local` re-mirrors each `exported` slot on
    /// `module_env`, so an aliased export reflects later writes to its
    /// source binding (live binding, §16.2.1.7) rather than a one-time
    /// snapshot at the export statement.
    pub(crate) reexport_local_targets: HashMap<String, Vec<String>>,
    /// Per-request resolved target URL — populated by the host
    /// before module compilation begins. The compiler emits the
    /// pre-resolved (referrer, request, target) row into the produced
    /// fragment's `module_resolutions` table.
    pub(crate) pre_resolved_imports: HashMap<ImportRequest, String>,
    /// Requests imported via `import defer * as ns from "x"` →
    /// dedicated record number of the *deferred* namespace. Kept
    /// separate from `import_records` so an eager `import * as a` and a
    /// deferred `import defer * as b` of the same module bind to
    /// distinct objects (§16.2.1 deferred namespaces are distinct from
    /// eager ones). Two deferred imports of the same module share one
    /// record, so their namespaces are identical.
    pub(crate) deferred_import_records: HashMap<ImportRequest, u16>,
}

/// Pre-resolved import / export information passed by the host
/// (typically the runtime's module-graph driver) into
/// [`compile_module_program`]. The compiler trusts the host for
/// resolution; it lowers identifier references against the
/// resolved structure.
#[derive(Debug, Clone, Default)]
pub struct ModuleHostInfo {
    /// Canonical URL of this module (e.g.,
    /// `"file:///abs/path/to/main.ts"`).
    pub module_url: String,
    /// Request → target URL pairs — every request the module makes
    /// in a static `import` or literal-string `import("./x")` must be
    /// present.
    pub resolved_imports: std::collections::HashMap<ImportRequest, String>,
}

/// Module-level mutable state shared across nested function
/// compilations. Threaded as `Rc<RefCell<ModuleBuilder>>` so the
/// `<main>` context and any nested function context can intern
/// constants into the same pool and register their `Function`
/// records into the same table without contorting the borrow
/// checker.
#[derive(Debug, Default)]
pub(crate) struct ModuleBuilder {
    pub(crate) functions: Vec<Function>,
    pub(crate) constants: Vec<Constant>,
    /// Content hash → index of the first pool entry with that hash.
    constant_index: rustc_hash::FxHashMap<u64, u32>,
    /// Code units of the string being interned, reused from one string to
    /// the next so a string already in the pool costs no allocation.
    string_units: Vec<u16>,
    /// Instruction and span buffers a finished function left empty, for
    /// the next function to emit into: a function's buffers grow while it
    /// compiles and are copied out exactly when it finishes.
    spare_buffers: Vec<(
        otter_bytecode::FunctionCodeBuilder,
        Vec<otter_bytecode::SpanEntry>,
    )>,
    /// §13.2.8.4 — tagged-template sites, one per Parse Node.
    pub(crate) template_sites: Vec<otter_bytecode::TemplateSite>,
    /// Monotonic counter handed out by `compile_class` so each
    /// lexical class declaration owns a private-field namespace
    /// distinct from every other class — `class A { #x }` and
    /// `class B { #x }` mangle to different runtime keys, matching
    /// §15.7.1 PrivateName uniqueness.
    pub(crate) next_private_namespace: u32,
}

impl ModuleBuilder {
    /// Index of `constant` in the pool, appending it when no equal entry
    /// exists. Equal entries resolve to the lowest index.
    pub(crate) fn intern_constant(&mut self, constant: Constant) -> u32 {
        let hash = constant_hash(&constant);
        match self.constant_index.get(&hash) {
            Some(&index) if self.constants[index as usize] == constant => return index,
            // A 64-bit hash collision with different content: the pool scan
            // stays the authority.
            Some(_) => {
                if let Some(index) = self.constants.iter().position(|c| *c == constant) {
                    return index as u32;
                }
            }
            None => {}
        }
        self.push_constant(constant)
    }

    /// [`Self::intern_constant`] of the string constant `value`, which
    /// allocates only when `value` is not in the pool yet.
    pub(crate) fn intern_str_constant(&mut self, value: &str) -> u32 {
        let mut units = std::mem::take(&mut self.string_units);
        units.clear();
        units.extend(value.encode_utf16());
        let hash = string_hash(&units);
        let found = match self.constant_index.get(&hash) {
            Some(&index) => match &self.constants[index as usize] {
                Constant::String { utf16 } if *utf16 == units => Some(index),
                _ => self
                    .constants
                    .iter()
                    .position(|constant| {
                        matches!(constant, Constant::String { utf16 } if *utf16 == units)
                    })
                    .map(|index| index as u32),
            },
            None => None,
        };
        let index = found.unwrap_or_else(|| {
            self.push_constant(Constant::String {
                utf16: units.clone(),
            })
        });
        self.string_units = units;
        index
    }

    /// Empty instruction and span buffers for a function to emit into.
    pub(crate) fn code_buffers(
        &mut self,
    ) -> (
        otter_bytecode::FunctionCodeBuilder,
        Vec<otter_bytecode::SpanEntry>,
    ) {
        self.spare_buffers.pop().unwrap_or_default()
    }

    /// Keep a finished function's emptied buffers for the next one.
    pub(crate) fn recycle_code_buffers(
        &mut self,
        code: otter_bytecode::FunctionCodeBuilder,
        mut spans: Vec<otter_bytecode::SpanEntry>,
    ) {
        spans.clear();
        self.spare_buffers.push((code, spans));
    }

    /// Append `constant` without deduplication and return its index.
    pub(crate) fn push_constant(&mut self, constant: Constant) -> u32 {
        let index = u32::try_from(self.constants.len()).expect("constant pool exceeds u32");
        self.constant_index
            .entry(constant_hash(&constant))
            .or_insert(index);
        self.constants.push(constant);
        index
    }
}

/// [`constant_hash`] of a string constant holding `units`.
fn string_hash(units: &[u16]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    std::mem::discriminant(&Constant::String { utf16: Vec::new() }).hash(&mut hasher);
    units.hash(&mut hasher);
    hasher.finish()
}

fn constant_hash(constant: &Constant) -> u64 {
    use std::hash::{Hash, Hasher};
    if let Constant::String { utf16 } = constant {
        return string_hash(utf16);
    }
    let mut hasher = rustc_hash::FxHasher::default();
    std::mem::discriminant(constant).hash(&mut hasher);
    match constant {
        Constant::String { .. } => unreachable!("hashed above"),
        Constant::Number { bits } => bits.hash(&mut hasher),
        Constant::FunctionId { index } => index.hash(&mut hasher),
        Constant::BigInt { decimal } => decimal.hash(&mut hasher),
        Constant::RegExp {
            pattern_utf16,
            flags,
        } => {
            pattern_utf16.hash(&mut hasher);
            flags.hash(&mut hasher);
        }
        Constant::ArrayBoilerplate { elements } => elements.hash(&mut hasher),
    }
    hasher.finish()
}

/// The import binding `name` names in the enclosing module, if any.
pub(crate) fn find_module_import_binding(cx: &Compiler, name: &str) -> Option<ImportBinding> {
    // An import binding lives in the module environment, the outermost
    // scope a module's code sees: any declaration of the same name between
    // here and there — a parameter, a `let`, a nested function's `var` —
    // shadows it (§9.1.2.1 GetIdentifierReference). Minified modules reuse
    // one-letter names for both all the time.
    // Most names are not imports: look the name up in the import table
    // first, and only then prove no nearer declaration shadows it.
    let binding = cx
        .stack
        .iter()
        .rev()
        .find_map(|frame| frame.module_state.as_ref()?.imported_names.get(name))?;
    if cx.resolve_name(name).is_some() {
        return None;
    }
    Some(binding.clone())
}

/// The module URL an import op names at run time. The host resolved the
/// request before compilation, so the op carries its target instead of a key
/// the VM would have to resolve a second time. A request no enclosing module
/// resolved (script-mode compilation) falls back to its raw specifier, which
/// names no module and so reports itself in the failure.
pub(crate) fn import_target_constant(cx: &Compiler, request: &ImportRequest) -> String {
    module_specifier_target(cx, request).unwrap_or_else(|| request.specifier.clone())
}

/// Resolve an import `request` to its canonical target URL via the
/// host-provided `pre_resolved_imports`, walking the context stack so a
/// nested function (whose own `module_state` is `None`) still finds the
/// enclosing module's resolution table. Returns `None` when no enclosing
/// module recorded the request (e.g. script-mode compilation).
pub(crate) fn module_specifier_target(cx: &Compiler, request: &ImportRequest) -> Option<String> {
    for frame in cx.stack.iter().rev() {
        if let Some(state) = &frame.module_state
            && let Some(target) = state.pre_resolved_imports.get(request)
        {
            return Some(target.clone());
        }
    }
    None
}

pub(crate) fn bytecode_source_kind(kind: SyntaxSourceKind) -> BytecodeSourceKind {
    if kind.is_typescript() {
        BytecodeSourceKind::TypeScript
    } else {
        BytecodeSourceKind::JavaScript
    }
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
