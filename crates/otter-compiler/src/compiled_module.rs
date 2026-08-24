//! Compiler-owned runtime boundary metadata.
//!
//! `BytecodeModule` remains the VM execution payload. This module owns the
//! higher-level `ResolvedSource -> CompiledModule` contract: bytecode plus the
//! source spans, import records, export records, and live-binding labels that
//! the runtime needs for diagnostics, source-map registration, and dumps.
//!
//! # Contents
//! - [`CompiledModule`] wraps VM bytecode with [`CompiledModuleMetadata`].
//! - [`CompiledFunctionSpans`] groups source spans under one function identity.
//! - [`CompiledImport`], [`CompiledExport`], and [`LiveBindingSlot`] describe
//!   module-surface metadata emitted from the OXC AST.
//! - [`collect_module_metadata`] extracts metadata without string parsing.
//!
//! # Invariants
//! - Metadata is derived from the same OXC AST and bytecode the compiler
//!   emits; no regex or source-string parsing is used.
//! - Span ranges point into the original source text offsets.
//! - Diagnostic strings are owned once per function and complete metadata
//!   materialization is checked, hard-bounded, and fallibly reserved.
//! - Live-binding slots are deterministic and sorted by exported name.
//!
//! # See also
//! - [`crate::compile_module_program_to_module`]
//! - [`crate::ModuleHostInfo`]

use std::collections::{BTreeSet, HashMap, HashSet};

use otter_bytecode::{BytecodeModule, SourceKind as BytecodeSourceKind, SpanEntry};
use oxc_ast::ast::{Expression, Program};
use oxc_ast_visit::Visit;
use serde::{Deserialize, Serialize};

use crate::{CompileError, ImportRequest, ModuleHostInfo, module_export_name_to_str};

/// Hard upper bound for the compiler-owned source-map DTO.
///
/// This is deliberately below the decoded bytecode admission ceiling. A
/// hostile cache entry or generated source cannot use otherwise-valid span
/// records to trigger an unbounded second materialization at the runtime
/// boundary.
pub const MAX_COMPILED_METADATA_BYTES: usize = 32 * 1024 * 1024;

/// Frozen compiler/runtime boundary product for one source module.
///
/// The VM still executes [`BytecodeModule`]. Runtime-facing callers use this
/// wrapper so source spans and module import/export metadata travel with the
/// compiled bytecode instead of being rediscovered later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledModule {
    /// VM bytecode payload.
    pub bytecode: BytecodeModule,
    /// Source-level metadata owned by the compiler output.
    pub metadata: CompiledModuleMetadata,
}

impl CompiledModule {
    /// Build a compiled module from bytecode and compiler metadata.
    #[must_use]
    pub const fn new(bytecode: BytecodeModule, metadata: CompiledModuleMetadata) -> Self {
        Self { bytecode, metadata }
    }

    /// Build a compiled module whose metadata is derived from bytecode spans.
    ///
    /// # Errors
    /// Returns [`CompileError::MetadataLimit`] when the compact source-map DTO
    /// exceeds its hard budget, or [`CompileError::MetadataAllocation`] when a
    /// bounded reservation fails.
    pub fn from_bytecode(bytecode: BytecodeModule) -> Result<Self, CompileError> {
        let metadata = CompiledModuleMetadata::span_only_from_bytecode(&bytecode)?;
        Ok(Self { bytecode, metadata })
    }

    /// Split into the VM bytecode payload.
    #[must_use]
    pub fn into_bytecode(self) -> BytecodeModule {
        self.bytecode
    }
}

/// Metadata emitted alongside compiled bytecode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompiledModuleMetadata {
    /// Canonical source URL or caller-supplied module specifier.
    pub source_url: String,
    /// JavaScript or TypeScript source family used for bytecode emission.
    pub source_kind: BytecodeSourceKind,
    /// Source spans grouped by function so diagnostic strings are owned once
    /// per function rather than once per program counter.
    pub function_spans: Vec<CompiledFunctionSpans>,
    /// Static and literal-dynamic import edges observed in the source.
    pub imports: Vec<CompiledImport>,
    /// Export entries observed in the source.
    pub exports: Vec<CompiledExport>,
    /// Deterministic live-binding slot labels for module exports.
    pub live_binding_slots: Vec<LiveBindingSlot>,
    /// Named import requests (`import { x as y } from "m"` and
    /// `import d from "m"`) used for link-time ResolveExport
    /// validation. Namespace (`import * as ns`) and bare side-effect
    /// imports carry no binding name and are not recorded here.
    #[serde(default)]
    pub named_imports: Vec<NamedImport>,
    /// §16.2.1.6 ResolveExport result table for this module, computed
    /// at link time over the whole graph. Maps each exported name in
    /// the module's §16.2.1.7 GetExportedNames set (the unambiguous
    /// ones) to the live binding it resolves to. The runtime's Module
    /// Namespace Exotic Object reads and `Op::LoadImportBinding`
    /// consult this so re-exported / star-exported names read the
    /// *defining* module's live environment instead of a snapshot.
    /// Empty for unlinked single-module compiler output.
    #[serde(default)]
    pub resolved_exports: std::collections::BTreeMap<String, ResolvedBinding>,
}

/// One §16.2.1.6 ResolveExport result: an exported name resolves to
/// the binding `binding` in module `defining_module`. The sentinel
/// `binding == "*namespace*"` (used by `export * as ns from "m"`)
/// means the resolved value is `defining_module`'s namespace object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedBinding {
    /// Canonical URL of the module that owns the live binding.
    pub defining_module: String,
    /// Exported name under which `defining_module` holds the binding
    /// live on its environment, or `"*namespace*"`.
    pub binding: String,
}

/// A single import binding, used by the linker to validate named
/// imports (§16.2.1.6 ResolveExport) and to resolve local re-exports
/// (`export { x }` where `x` is itself imported) through their source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedImport {
    /// Raw source specifier of the importing declaration.
    pub specifier: String,
    /// `type` import attribute of the declaration, when it carries
    /// one — the other half of the key that names this import's
    /// target.
    #[serde(default)]
    pub attr_type: Option<String>,
    /// Imported export name (`"default"` for a default import, empty
    /// for a namespace import).
    pub name: String,
    /// Local binding alias introduced by this import.
    #[serde(default)]
    pub local: String,
    /// `true` for `import * as ns` (binds the module namespace object).
    #[serde(default)]
    pub is_namespace: bool,
    /// `true` for `import defer * as ns` — re-exporting this binding
    /// (`export { ns }`) preserves deferred namespace semantics
    /// (§16.2.1.6 ResolveExport: `[[BindingName]]` is
    /// `deferred-namespace`, not `namespace`).
    #[serde(default)]
    pub is_deferred: bool,
}

impl Default for CompiledModuleMetadata {
    fn default() -> Self {
        Self {
            source_url: String::new(),
            source_kind: BytecodeSourceKind::JavaScript,
            function_spans: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            live_binding_slots: Vec::new(),
            named_imports: Vec::new(),
            resolved_exports: std::collections::BTreeMap::new(),
        }
    }
}

impl CompiledModuleMetadata {
    /// Reconstruct diagnostics-only metadata from an immutable bytecode
    /// module.
    ///
    /// This is the cache-hit boundary for classic scripts: source spans,
    /// function names, and module URLs are fully represented in bytecode, so
    /// they can be restored without reparsing source. Import/export and live
    /// binding tables require the original module AST and remain empty.
    ///
    /// # Errors
    /// Returns [`CompileError::MetadataLimit`] when the compact source-map DTO
    /// exceeds its hard budget, or [`CompileError::MetadataAllocation`] when a
    /// bounded reservation fails.
    pub fn span_only_from_bytecode(bytecode: &BytecodeModule) -> Result<Self, CompileError> {
        Self::span_only_from_bytecode_with_budget(bytecode, MAX_COMPILED_METADATA_BYTES)
    }

    /// Reconstruct diagnostics metadata under a caller-selected stricter
    /// budget.
    ///
    /// `budget` is capped at [`MAX_COMPILED_METADATA_BYTES`]; callers can fail
    /// closed earlier but cannot bypass the engine-wide ceiling.
    ///
    /// # Errors
    /// Returns [`CompileError::MetadataLimit`] when materialization exceeds the
    /// effective budget, or [`CompileError::MetadataAllocation`] when a
    /// bounded reservation fails.
    pub fn span_only_from_bytecode_with_budget(
        bytecode: &BytecodeModule,
        budget: usize,
    ) -> Result<Self, CompileError> {
        Self::span_only_from_bytecode_with_budget_and_source(
            bytecode,
            &bytecode.module,
            bytecode.source_kind,
            budget.min(MAX_COMPILED_METADATA_BYTES),
        )
    }

    pub(crate) fn span_only_from_bytecode_with_source(
        bytecode: &BytecodeModule,
        source_url: &str,
        source_kind: BytecodeSourceKind,
    ) -> Result<Self, CompileError> {
        Self::span_only_from_bytecode_with_budget_and_source(
            bytecode,
            source_url,
            source_kind,
            MAX_COMPILED_METADATA_BYTES,
        )
    }

    fn span_only_from_bytecode_with_budget_and_source(
        bytecode: &BytecodeModule,
        source_url: &str,
        source_kind: BytecodeSourceKind,
        budget: usize,
    ) -> Result<Self, CompileError> {
        let materialized_bytes = compiled_metadata_materialized_bytes(bytecode, source_url)?;
        if materialized_bytes > budget {
            return Err(CompileError::MetadataLimit {
                requested_bytes: materialized_bytes,
                limit_bytes: budget,
            });
        }

        Ok(Self {
            source_url: try_clone_metadata_string(source_url, materialized_bytes)?,
            source_kind,
            function_spans: compiled_spans_from_bytecode(bytecode, materialized_bytes)?,
            imports: Vec::new(),
            exports: Vec::new(),
            live_binding_slots: Vec::new(),
            named_imports: Vec::new(),
            resolved_exports: std::collections::BTreeMap::new(),
        })
    }
}

/// Source spans owned by one compiled function.
///
/// Function and module strings are stored once regardless of how many PCs the
/// function maps. `spans` retains the bytecode order and contains only compact
/// numeric entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompiledFunctionSpans {
    /// Function id that owns every program counter in `spans`.
    pub function_id: u32,
    /// Function name for diagnostics and dumps.
    pub function_name: String,
    /// Module URL carried by the function, falling back to the top-level
    /// bytecode module name when the function is script-local.
    pub module_url: String,
    /// Compact program-counter/source-range entries for this function.
    pub spans: Vec<SpanEntry>,
}

/// Import metadata emitted by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompiledImport {
    /// Raw source specifier.
    pub specifier: String,
    /// `type` import attribute of the request, when it carries one.
    /// The specifier alone does not identify an edge: the same file
    /// imported under two types is two edges with two targets.
    #[serde(default)]
    pub attr_type: Option<String>,
    /// Host-resolved target URL when statically known.
    pub target: Option<String>,
    /// Import edge kind.
    pub kind: CompiledImportKind,
}

/// Import edge family.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CompiledImportKind {
    /// Static `import ... from` declaration.
    Static,
    /// Re-export source such as `export * from`.
    ReExport,
    /// Literal dynamic import such as `import("./x")`.
    DynamicLiteral,
}

/// Export metadata emitted by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompiledExport {
    /// Exported name (`"default"`, `"*"`, or a named export).
    pub name: String,
    /// Local binding name when the export maps to one.
    pub local: Option<String>,
    /// Re-export source specifier when this export forwards another module.
    pub from: Option<String>,
    /// `type` import attribute the re-export source was written with.
    /// Half of the key naming the forwarded module, as it is on an
    /// import.
    #[serde(default)]
    pub from_attr_type: Option<String>,
}

/// Deterministic live-binding slot metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveBindingSlot {
    /// Exported binding name.
    pub name: String,
    /// Dense deterministic slot index for diagnostics/dumps.
    pub slot: u32,
}

/// Metadata pieces extracted from a module AST.
#[derive(Debug, Default)]
pub(crate) struct ModuleMetadataParts {
    /// Import metadata.
    pub(crate) imports: Vec<CompiledImport>,
    /// Export metadata.
    pub(crate) exports: Vec<CompiledExport>,
    /// Live-binding slot metadata.
    pub(crate) live_binding_slots: Vec<LiveBindingSlot>,
    /// Named import requests for link-time resolution validation.
    pub(crate) named_imports: Vec<NamedImport>,
}

pub(crate) fn collect_module_metadata(
    program: &Program<'_>,
    host: &ModuleHostInfo,
) -> ModuleMetadataParts {
    let mut visitor = ModuleMetadataVisitor {
        resolved_imports: &host.resolved_imports,
        imports: Vec::new(),
        exports: Vec::new(),
        named_imports: Vec::new(),
        live_binding_names: BTreeSet::new(),
        seen_imports: HashSet::new(),
    };
    for stmt in &program.body {
        visitor.visit_statement(stmt);
    }
    let live_binding_slots = visitor
        .live_binding_names
        .into_iter()
        .enumerate()
        .map(|(slot, name)| LiveBindingSlot {
            name,
            slot: slot as u32,
        })
        .collect();
    ModuleMetadataParts {
        imports: visitor.imports,
        exports: visitor.exports,
        live_binding_slots,
        named_imports: visitor.named_imports,
    }
}

fn compiled_metadata_materialized_bytes(
    bytecode: &BytecodeModule,
    source_url: &str,
) -> Result<usize, CompileError> {
    let mut bytes = source_url.len();
    for function in bytecode
        .functions
        .iter()
        .filter(|function| !function.spans.is_empty())
    {
        let module_url = if function.module_url.is_empty() {
            bytecode.module.as_str()
        } else {
            function.module_url.as_str()
        };
        bytes = bytes
            .checked_add(std::mem::size_of::<CompiledFunctionSpans>())
            .and_then(|bytes| bytes.checked_add(function.name.len()))
            .and_then(|bytes| bytes.checked_add(module_url.len()))
            .and_then(|bytes| {
                function
                    .spans
                    .len()
                    .checked_mul(std::mem::size_of::<SpanEntry>())
                    .and_then(|span_bytes| bytes.checked_add(span_bytes))
            })
            .ok_or(CompileError::MetadataLimit {
                requested_bytes: usize::MAX,
                limit_bytes: MAX_COMPILED_METADATA_BYTES,
            })?;
    }
    Ok(bytes)
}

fn compiled_spans_from_bytecode(
    bytecode: &BytecodeModule,
    materialized_bytes: usize,
) -> Result<Vec<CompiledFunctionSpans>, CompileError> {
    let function_count = bytecode
        .functions
        .iter()
        .filter(|function| !function.spans.is_empty())
        .count();
    let mut function_spans = Vec::new();
    function_spans
        .try_reserve_exact(function_count)
        .map_err(|_| CompileError::MetadataAllocation {
            requested_bytes: materialized_bytes,
        })?;

    for function in bytecode
        .functions
        .iter()
        .filter(|function| !function.spans.is_empty())
    {
        let module_url = if function.module_url.is_empty() {
            bytecode.module.as_str()
        } else {
            function.module_url.as_str()
        };
        let mut spans = Vec::new();
        spans.try_reserve_exact(function.spans.len()).map_err(|_| {
            CompileError::MetadataAllocation {
                requested_bytes: materialized_bytes,
            }
        })?;
        spans.extend_from_slice(&function.spans);
        function_spans.push(CompiledFunctionSpans {
            function_id: function.id,
            function_name: try_clone_metadata_string(&function.name, materialized_bytes)?,
            module_url: try_clone_metadata_string(module_url, materialized_bytes)?,
            spans,
        });
    }
    Ok(function_spans)
}

fn try_clone_metadata_string(
    value: &str,
    materialized_bytes: usize,
) -> Result<String, CompileError> {
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|_| CompileError::MetadataAllocation {
            requested_bytes: materialized_bytes,
        })?;
    cloned.push_str(value);
    Ok(cloned)
}

struct ModuleMetadataVisitor<'a> {
    resolved_imports: &'a HashMap<ImportRequest, String>,
    imports: Vec<CompiledImport>,
    exports: Vec<CompiledExport>,
    named_imports: Vec<NamedImport>,
    live_binding_names: BTreeSet<String>,
    seen_imports: HashSet<(ImportRequest, CompiledImportKind)>,
}

impl ModuleMetadataVisitor<'_> {
    fn record_import(&mut self, request: ImportRequest, kind: CompiledImportKind) {
        let target = self.resolved_imports.get(&request).cloned();
        if !self.seen_imports.insert((request.clone(), kind)) {
            return;
        }
        self.imports.push(CompiledImport {
            specifier: request.specifier,
            attr_type: request.attr_type,
            target,
            kind,
        });
    }

    fn record_export(&mut self, name: String, local: Option<String>, from: Option<ImportRequest>) {
        self.live_binding_names.insert(name.clone());
        let (from, from_attr_type) = match from {
            Some(request) => (Some(request.specifier), request.attr_type),
            None => (None, None),
        };
        self.exports.push(CompiledExport {
            name,
            local,
            from,
            from_attr_type,
        });
    }
}

impl<'a> Visit<'a> for ModuleMetadataVisitor<'_> {
    fn visit_import_declaration(&mut self, decl: &oxc_ast::ast::ImportDeclaration<'a>) {
        if decl.import_kind.is_type() {
            return;
        }
        let specifier = decl.source.value.as_str();
        let attr_type = crate::import_attribute_type(decl.with_clause.as_deref());
        self.record_import(
            ImportRequest::new(specifier, attr_type.clone()),
            CompiledImportKind::Static,
        );
        // Record each *named* binding request (`import { x as y }`
        // and `import d`) for link-time ResolveExport validation.
        // Namespace (`import * as ns`) carries no single export name.
        let is_deferred = matches!(decl.phase, Some(oxc_ast::ast::ImportPhase::Defer));
        if let Some(specifiers) = &decl.specifiers {
            for spec in specifiers {
                use oxc_ast::ast::ImportDeclarationSpecifier as Spec;
                let (name, local, is_namespace) = match spec {
                    Spec::ImportSpecifier(s) => (
                        module_export_name_to_str(&s.imported),
                        s.local.name.as_str().to_string(),
                        false,
                    ),
                    Spec::ImportDefaultSpecifier(s) => (
                        "default".to_string(),
                        s.local.name.as_str().to_string(),
                        false,
                    ),
                    Spec::ImportNamespaceSpecifier(s) => {
                        (String::new(), s.local.name.as_str().to_string(), true)
                    }
                };
                self.named_imports.push(NamedImport {
                    specifier: specifier.to_string(),
                    attr_type: attr_type.clone(),
                    name,
                    local,
                    is_namespace,
                    is_deferred,
                });
            }
        }
    }

    fn visit_export_named_declaration(&mut self, decl: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
        if decl.export_kind.is_type() {
            return;
        }
        let from = decl.source.as_ref().map(|src| {
            ImportRequest::new(
                src.value.as_str(),
                crate::import_attribute_type(decl.with_clause.as_deref()),
            )
        });
        if let Some(request) = &from {
            self.record_import(request.clone(), CompiledImportKind::ReExport);
        }
        if let Some(inner) = &decl.declaration {
            record_exports_from_declaration(self, inner, None);
        }
        for spec in &decl.specifiers {
            let exported = module_export_name_to_str(&spec.exported);
            let local = Some(module_export_name_to_str(&spec.local));
            self.record_export(exported, local, from.clone());
        }
        oxc_ast_visit::walk::walk_export_named_declaration(self, decl);
    }

    fn visit_export_all_declaration(&mut self, decl: &oxc_ast::ast::ExportAllDeclaration<'a>) {
        if decl.export_kind.is_type() {
            return;
        }
        let source = ImportRequest::new(
            decl.source.value.as_str(),
            crate::import_attribute_type(decl.with_clause.as_deref()),
        );
        self.record_import(source.clone(), CompiledImportKind::ReExport);
        let exported = decl
            .exported
            .as_ref()
            .map(module_export_name_to_str)
            .unwrap_or_else(|| "*".to_string());
        self.record_export(exported, None, Some(source));
    }

    fn visit_export_default_declaration(
        &mut self,
        decl: &oxc_ast::ast::ExportDefaultDeclaration<'a>,
    ) {
        let local = match &decl.declaration {
            oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                function.id.as_ref().map(|id| id.name.as_str().to_string())
            }
            oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                class.id.as_ref().map(|id| id.name.as_str().to_string())
            }
            _ => None,
        };
        self.record_export("default".to_string(), local, None);
        oxc_ast_visit::walk::walk_export_default_declaration(self, decl);
    }

    fn visit_import_expression(&mut self, imp: &oxc_ast::ast::ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &imp.source {
            self.record_import(
                ImportRequest::plain(lit.value.as_str()),
                CompiledImportKind::DynamicLiteral,
            );
        }
        oxc_ast_visit::walk::walk_import_expression(self, imp);
    }
}

fn record_exports_from_declaration(
    visitor: &mut ModuleMetadataVisitor<'_>,
    decl: &oxc_ast::ast::Declaration<'_>,
    from: Option<ImportRequest>,
) {
    match decl {
        oxc_ast::ast::Declaration::VariableDeclaration(var_decl) => {
            // §16.2.3.2 ExportedBindings — BoundNames of each
            // declarator, walking destructuring patterns so
            // `export const { check } = …` records `check`.
            for declarator in &var_decl.declarations {
                let mut names = Vec::new();
                crate::hoist::collect_pattern_var_names(&declarator.id, &mut names);
                for name in names {
                    visitor.record_export(name.clone(), Some(name), from.clone());
                }
            }
        }
        oxc_ast::ast::Declaration::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                let name = id.name.as_str().to_string();
                visitor.record_export(name.clone(), Some(name), from);
            }
        }
        oxc_ast::ast::Declaration::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                let name = id.name.as_str().to_string();
                visitor.record_export(name.clone(), Some(name), from);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModuleHostInfo, compile_module_program_to_module, compile_script_source};
    use otter_syntax::{SourceKind as SyntaxSourceKind, with_program};

    fn host_info(specifiers: &[(&str, &str)]) -> ModuleHostInfo {
        ModuleHostInfo {
            module_url: "file:///test/main.ts".to_string(),
            resolved_imports: specifiers
                .iter()
                .map(|(specifier, target)| (ImportRequest::plain(*specifier), target.to_string()))
                .collect(),
        }
    }

    #[test]
    fn compiled_module_emits_import_export_and_span_metadata() {
        let src = r#"
            import { value } from "./other.ts";
            export const answer = value + 1;
            export { answer as renamed };
            import("./lazy.ts");
        "#;
        let host = host_info(&[
            ("./other.ts", "file:///test/other.ts"),
            ("./lazy.ts", "file:///test/lazy.ts"),
        ]);
        let compiled = with_program(src, SyntaxSourceKind::TypeScript, |program| {
            compile_module_program_to_module(program, SyntaxSourceKind::TypeScript, &host)
        })
        .unwrap()
        .unwrap();

        assert_eq!(compiled.metadata.source_url, "file:///test/main.ts");
        assert_eq!(
            compiled.metadata.source_kind,
            BytecodeSourceKind::TypeScript
        );
        assert!(
            compiled
                .metadata
                .imports
                .iter()
                .any(|import| import.specifier == "./other.ts"
                    && import.target.as_deref() == Some("file:///test/other.ts")
                    && import.kind == CompiledImportKind::Static)
        );
        assert!(
            compiled
                .metadata
                .imports
                .iter()
                .any(|import| import.specifier == "./lazy.ts"
                    && import.target.as_deref() == Some("file:///test/lazy.ts")
                    && import.kind == CompiledImportKind::DynamicLiteral)
        );
        assert!(
            compiled
                .metadata
                .exports
                .iter()
                .any(|export| export.name == "answer" && export.local.as_deref() == Some("answer"))
        );
        assert!(
            compiled
                .metadata
                .exports
                .iter()
                .any(|export| export.name == "renamed"
                    && export.local.as_deref() == Some("answer"))
        );
        assert!(
            compiled
                .metadata
                .live_binding_slots
                .iter()
                .any(|slot| slot.name == "answer")
        );
        assert!(!compiled.metadata.function_spans.is_empty());
    }

    #[test]
    fn borrowed_program_module_api_emits_metadata_without_parse_wrapper() {
        let src = r#"
            import { value } from "./other.ts";
            export const answer = value + 1;
            import("./lazy.ts");
        "#;
        let host = host_info(&[
            ("./other.ts", "file:///test/other.ts"),
            ("./lazy.ts", "file:///test/lazy.ts"),
        ]);
        let compiled = with_program(src, SyntaxSourceKind::TypeScript, |program| {
            compile_module_program_to_module(program, SyntaxSourceKind::TypeScript, &host)
        })
        .unwrap()
        .unwrap();

        assert_eq!(compiled.metadata.source_url, "file:///test/main.ts");
        assert_eq!(compiled.metadata.imports.len(), 2);
        assert!(
            compiled
                .metadata
                .exports
                .iter()
                .any(|export| export.name == "answer")
        );
        assert!(
            compiled
                .metadata
                .live_binding_slots
                .iter()
                .any(|slot| slot.name == "answer")
        );
        assert_eq!(compiled.bytecode.module, "file:///test/main.ts");
        assert_eq!(
            compiled.bytecode.module_resolutions.len(),
            2,
            "host imports should be preserved in bytecode metadata"
        );
    }

    #[test]
    fn compact_span_metadata_matches_each_bytecode_function_exactly() {
        let bytecode = compile_script_source(
            "function add(a, b) { return a + b; } add(20, 22);",
            SyntaxSourceKind::JavaScript,
            "file:///compact.js",
        )
        .expect("representative script compiles");
        let metadata = CompiledModuleMetadata::span_only_from_bytecode(&bytecode)
            .expect("ordinary metadata fits its budget");

        let expected: Vec<_> = bytecode
            .functions
            .iter()
            .filter(|function| !function.spans.is_empty())
            .map(|function| {
                (
                    function.id,
                    function.name.as_str(),
                    if function.module_url.is_empty() {
                        bytecode.module.as_str()
                    } else {
                        function.module_url.as_str()
                    },
                    function.spans.as_slice(),
                )
            })
            .collect();
        let actual: Vec<_> = metadata
            .function_spans
            .iter()
            .map(|function| {
                (
                    function.function_id,
                    function.function_name.as_str(),
                    function.module_url.as_str(),
                    function.spans.as_slice(),
                )
            })
            .collect();

        assert_eq!(actual, expected);
        assert_eq!(metadata.source_url, bytecode.module);
        assert_eq!(metadata.source_kind, bytecode.source_kind);
    }

    #[test]
    fn compact_helper_and_explicit_default_budget_are_equivalent() {
        let bytecode = compile_script_source(
            "const answer = 42; answer;",
            SyntaxSourceKind::JavaScript,
            "file:///equivalent.js",
        )
        .expect("ordinary script compiles");

        let ordinary = CompiledModuleMetadata::span_only_from_bytecode(&bytecode)
            .expect("ordinary helper succeeds");
        let explicit = CompiledModuleMetadata::span_only_from_bytecode_with_budget_and_source(
            &bytecode,
            &bytecode.module,
            bytecode.source_kind,
            MAX_COMPILED_METADATA_BYTES,
        )
        .expect("explicit default budget succeeds");

        assert_eq!(ordinary, explicit);
    }

    #[test]
    fn compact_span_metadata_serializes_one_identity_per_function() {
        let bytecode = compile_script_source(
            "function twice(value) { return value * 2; } twice(21);",
            SyntaxSourceKind::JavaScript,
            "file:///serialized.js",
        )
        .expect("representative script compiles");
        let metadata = CompiledModuleMetadata::span_only_from_bytecode(&bytecode)
            .expect("ordinary metadata fits its budget");

        let encoded = serde_json::to_value(&metadata).expect("metadata serializes");
        assert!(encoded.get("spans").is_none(), "flat schema was removed");
        let groups = encoded["function_spans"]
            .as_array()
            .expect("grouped function span array");
        assert_eq!(groups.len(), metadata.function_spans.len());
        for group in groups {
            assert!(group.get("function_name").is_some());
            assert!(group.get("module_url").is_some());
            for span in group["spans"].as_array().expect("compact spans") {
                assert!(span.get("function_name").is_none());
                assert!(span.get("module_url").is_none());
            }
        }

        let decoded: CompiledModuleMetadata =
            serde_json::from_value(encoded).expect("grouped metadata deserializes");
        assert_eq!(decoded, metadata);
    }

    #[test]
    fn long_diagnostic_strings_and_many_spans_fail_the_checked_budget() {
        let mut bytecode =
            compile_script_source("0;", SyntaxSourceKind::JavaScript, "file:///seed.js")
                .expect("seed script compiles");
        bytecode.module = format!("file:///{}.js", "m".repeat(4_096));
        let function = bytecode.functions.first_mut().expect("main function");
        function.name = "n".repeat(4_096);
        function.module_url = format!("file:///{}.js", "u".repeat(4_096));
        function.spans = (0..2_048_u32)
            .map(|pc| SpanEntry {
                pc,
                span: (pc, pc.saturating_add(1)),
            })
            .collect();

        let error = CompiledModuleMetadata::span_only_from_bytecode_with_budget_and_source(
            &bytecode,
            &bytecode.module,
            bytecode.source_kind,
            1_024,
        )
        .expect_err("hostile metadata must be rejected before materialization");

        assert!(matches!(
            error,
            CompileError::MetadataLimit {
                requested_bytes,
                limit_bytes: 1_024,
            } if requested_bytes > 1_024
        ));
        assert_eq!(bytecode.functions[0].spans.len(), 2_048);
        assert_eq!(bytecode.functions[0].name.len(), 4_096);
    }
}
