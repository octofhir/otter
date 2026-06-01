//! Compiler-owned runtime boundary metadata.
//!
//! `BytecodeModule` remains the VM execution payload. This module owns the
//! higher-level `ResolvedSource -> CompiledModule` contract: bytecode plus the
//! source spans, import records, export records, and live-binding labels that
//! the runtime needs for diagnostics, source-map registration, and dumps.
//!
//! # Contents
//! - [`CompiledModule`] wraps VM bytecode with [`CompiledModuleMetadata`].
//! - [`CompiledSourceSpan`] pins source spans to function ids and PCs.
//! - [`CompiledImport`], [`CompiledExport`], and [`LiveBindingSlot`] describe
//!   module-surface metadata emitted from the OXC AST.
//! - [`collect_module_metadata`] extracts metadata without string parsing.
//!
//! # Invariants
//! - Metadata is derived from the same OXC AST and bytecode the compiler
//!   emits; no regex or source-string parsing is used.
//! - Span ranges point into the original source text offsets.
//! - Live-binding slots are deterministic and sorted by exported name.
//!
//! # See also
//! - [`crate::compile_module_program_to_module`]
//! - [`crate::ModuleHostInfo`]

use std::collections::{BTreeSet, HashMap, HashSet};

use otter_bytecode::{BytecodeModule, SourceKind as BytecodeSourceKind};
use oxc_ast::ast::{Expression, Program};
use oxc_ast_visit::Visit;
use serde::{Deserialize, Serialize};

use crate::{ModuleHostInfo, module_export_name_to_str};

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
    #[must_use]
    pub fn from_bytecode(bytecode: BytecodeModule) -> Self {
        let source_url = bytecode.module.clone();
        let source_kind = bytecode.source_kind;
        let metadata = CompiledModuleMetadata::from_bytecode(&bytecode, source_url, source_kind);
        Self { bytecode, metadata }
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
    /// Source-span table owned by the compiled module.
    pub spans: Vec<CompiledSourceSpan>,
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
}

/// A single named import binding request, used by the linker to
/// validate that the imported name resolves in the target module
/// (§16.2.1.6 ResolveExport).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedImport {
    /// Raw source specifier of the importing declaration.
    pub specifier: String,
    /// Imported export name (`"default"` for a default import).
    pub name: String,
}

impl Default for CompiledModuleMetadata {
    fn default() -> Self {
        Self {
            source_url: String::new(),
            source_kind: BytecodeSourceKind::JavaScript,
            spans: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            live_binding_slots: Vec::new(),
            named_imports: Vec::new(),
        }
    }
}

impl CompiledModuleMetadata {
    pub(crate) fn from_bytecode(
        bytecode: &BytecodeModule,
        source_url: String,
        source_kind: BytecodeSourceKind,
    ) -> Self {
        Self {
            source_url,
            source_kind,
            spans: compiled_spans_from_bytecode(bytecode),
            imports: Vec::new(),
            exports: Vec::new(),
            live_binding_slots: Vec::new(),
            named_imports: Vec::new(),
        }
    }
}

/// One source span attached to a bytecode program counter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompiledSourceSpan {
    /// Function id that owns the program counter.
    pub function_id: u32,
    /// Function name for diagnostics and dumps.
    pub function_name: String,
    /// Module URL carried by the function, falling back to the top-level
    /// bytecode module name when the function is script-local.
    pub module_url: String,
    /// Program counter.
    pub pc: u32,
    /// Byte offset range into the original source.
    pub span: (u32, u32),
}

/// Import metadata emitted by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompiledImport {
    /// Raw source specifier.
    pub specifier: String,
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

fn compiled_spans_from_bytecode(bytecode: &BytecodeModule) -> Vec<CompiledSourceSpan> {
    let mut spans = Vec::new();
    for function in &bytecode.functions {
        let module_url = if function.module_url.is_empty() {
            bytecode.module.clone()
        } else {
            function.module_url.clone()
        };
        spans.extend(function.spans.iter().map(|entry| CompiledSourceSpan {
            function_id: function.id,
            function_name: function.name.clone(),
            module_url: module_url.clone(),
            pc: entry.pc,
            span: entry.span,
        }));
    }
    spans
}

struct ModuleMetadataVisitor<'a> {
    resolved_imports: &'a HashMap<String, String>,
    imports: Vec<CompiledImport>,
    exports: Vec<CompiledExport>,
    named_imports: Vec<NamedImport>,
    live_binding_names: BTreeSet<String>,
    seen_imports: HashSet<(String, CompiledImportKind)>,
}

impl ModuleMetadataVisitor<'_> {
    fn record_import(&mut self, specifier: &str, kind: CompiledImportKind) {
        let key = (specifier.to_string(), kind);
        if !self.seen_imports.insert(key) {
            return;
        }
        self.imports.push(CompiledImport {
            specifier: specifier.to_string(),
            target: self.resolved_imports.get(specifier).cloned(),
            kind,
        });
    }

    fn record_export(&mut self, name: String, local: Option<String>, from: Option<String>) {
        self.live_binding_names.insert(name.clone());
        self.exports.push(CompiledExport { name, local, from });
    }
}

impl<'a> Visit<'a> for ModuleMetadataVisitor<'_> {
    fn visit_import_declaration(&mut self, decl: &oxc_ast::ast::ImportDeclaration<'a>) {
        if decl.import_kind.is_type() {
            return;
        }
        let specifier = decl.source.value.as_str();
        self.record_import(specifier, CompiledImportKind::Static);
        // Record each *named* binding request (`import { x as y }`
        // and `import d`) for link-time ResolveExport validation.
        // Namespace (`import * as ns`) carries no single export name.
        if let Some(specifiers) = &decl.specifiers {
            for spec in specifiers {
                use oxc_ast::ast::ImportDeclarationSpecifier as Spec;
                let name = match spec {
                    Spec::ImportSpecifier(s) => module_export_name_to_str(&s.imported),
                    Spec::ImportDefaultSpecifier(_) => "default".to_string(),
                    Spec::ImportNamespaceSpecifier(_) => continue,
                };
                self.named_imports.push(NamedImport {
                    specifier: specifier.to_string(),
                    name,
                });
            }
        }
    }

    fn visit_export_named_declaration(&mut self, decl: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
        if decl.export_kind.is_type() {
            return;
        }
        let from = decl
            .source
            .as_ref()
            .map(|src| src.value.as_str().to_string());
        if let Some(specifier) = &from {
            self.record_import(specifier, CompiledImportKind::ReExport);
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
        let source = decl.source.value.as_str().to_string();
        self.record_import(&source, CompiledImportKind::ReExport);
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
            self.record_import(lit.value.as_str(), CompiledImportKind::DynamicLiteral);
        }
        oxc_ast_visit::walk::walk_import_expression(self, imp);
    }
}

fn record_exports_from_declaration(
    visitor: &mut ModuleMetadataVisitor<'_>,
    decl: &oxc_ast::ast::Declaration<'_>,
    from: Option<String>,
) {
    match decl {
        oxc_ast::ast::Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                    let name = id.name.as_str().to_string();
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
    use crate::{ModuleHostInfo, compile_module_program_to_module};
    use otter_syntax::{SourceKind as SyntaxSourceKind, with_program};

    fn host_info(specifiers: &[(&str, &str)]) -> ModuleHostInfo {
        ModuleHostInfo {
            module_url: "file:///test/main.ts".to_string(),
            resolved_imports: specifiers
                .iter()
                .map(|(specifier, target)| (specifier.to_string(), target.to_string()))
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
        assert!(!compiled.metadata.spans.is_empty());
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
}
