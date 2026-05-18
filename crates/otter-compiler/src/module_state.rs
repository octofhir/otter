//! ES-module lowering state and host-provided resolution metadata.
//!
//! # Contents
//! - import binding records
//! - module state maps
//! - module builder storage
//! - module source-kind helpers
//!
//! # Invariants
//! - Host-provided resolved imports are trusted by lowering.
//!
//! # See also
//! - `entry` for module compilation

use crate::*;

/// One pre-resolved import-record binding: maps an importer-side
/// alias (`import { a as alias } from "./other.ts"`) to the
/// import-record upvalue index plus the original source-side name
/// the property load reads.
#[derive(Debug, Clone)]
pub(crate) struct ImportBinding {
    /// Own-upvalue index of the `import_record_<n>` JsObject inside
    /// the running `<module-init>` frame.
    pub(crate) record_uv_idx: u16,
    /// Source-module name of the binding (e.g., the `a` in
    /// `import { a as alias } from "./other.ts"`). For default
    /// imports this is `"default"`. For namespace imports the
    /// alias resolves directly to the record itself; we store an
    /// empty string here as the sentinel.
    pub(crate) source_name: String,
    /// `true` for `import * as ns from "./..."` — the alias binds
    /// to the namespace JsObject directly, so reads return the
    /// record without an extra `LoadProperty`.
    pub(crate) is_namespace: bool,
}

/// Module-mode state attached to a [`FunctionContext`] when the
/// function is the top-level `<module-init>` of an ES-module
/// fragment.
#[derive(Debug, Default)]
pub(crate) struct ModuleState {
    /// Own-upvalue index of the `module_env` JsObject (param 0,
    /// hoisted into a cell at the top of the body so closures can
    /// capture it).
    pub(crate) module_env_uv: u16,
    /// Own-upvalue index of the `import_meta` JsObject (param 1).
    pub(crate) import_meta_uv: u16,
    /// Per-specifier upvalue index of the import-record JsObject.
    /// Populated by the import pre-pass at the start of the body.
    pub(crate) import_records: HashMap<String, u16>,
    /// Importer-side alias → import-record binding info.
    pub(crate) imported_names: HashMap<String, ImportBinding>,
    /// Names that this module exports. Every assignment to a name
    /// in this set emits an extra
    /// `StoreProperty module_env, name, value` after the regular
    /// store so live-binding writes propagate.
    pub(crate) exported_names: HashSet<String>,
    /// Per-specifier resolved target URL — populated by the host
    /// before module compilation begins. The compiler emits the
    /// pre-resolved (referrer, specifier, target) triple into the
    /// produced fragment's `module_resolutions` table.
    pub(crate) pre_resolved_imports: HashMap<String, String>,
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
    /// Specifier → target URL pairs — every specifier the
    /// module references in a static `import` or
    /// literal-string `import("./x")` must be present.
    pub resolved_imports: HashMap<String, String>,
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
    /// Monotonic counter handed out by `compile_class` so each
    /// lexical class declaration owns a private-field namespace
    /// distinct from every other class — `class A { #x }` and
    /// `class B { #x }` mangle to different runtime keys, matching
    /// §15.7.1 PrivateName uniqueness.
    pub(crate) next_private_namespace: u32,
}

/// alongside the synthetic upvalue name the inner function should
/// resolve via `resolve_capture` to land at the same record cell.
pub(crate) fn find_module_import_binding(
    cx: &Compiler,
    name: &str,
) -> Option<(ImportBinding, String)> {
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
