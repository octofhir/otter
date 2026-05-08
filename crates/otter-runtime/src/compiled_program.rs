//! Runtime-owned compiled program dump contract.
//!
//! The compiler emits [`otter_compiler::CompiledModule`] for one loaded source.
//! The runtime may link many compiled modules into one VM
//! [`otter_bytecode::BytecodeModule`]. This module owns the public dump DTO for
//! that runtime result: linked bytecode plus the per-source compiler metadata
//! retained for diagnostics and tooling.
//!
//! # Contents
//! - [`CompiledProgram`] — bytecode plus per-source metadata.
//! - constructors for script and linked-module graph outputs.
//!
//! # Invariants
//! - The bytecode field is the exact VM payload used for execution or checking.
//! - Metadata remains per original source module and is not reconstructed from
//!   linked bytecode after the fact.
//! - DTO fields are owned, serializable, and boundary-safe.
//!
//! # See also
//! - [`crate::module_graph::LinkedProgram`]
//! - [`otter_compiler::CompiledModule`]

use otter_bytecode::BytecodeModule;
use otter_compiler::{CompiledModule, CompiledModuleMetadata};
use otter_syntax::with_program;
use serde::{Deserialize, Serialize};

use crate::module_graph::LinkedProgram;
use crate::{
    OtterError, Runtime, SourceInput, module_loader, program_looks_like_module,
    source_path_has_module_extension, source_path_has_script_extension, source_path_package_type,
};

/// Runtime-compiled program ready for bytecode dumps or tooling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledProgram {
    /// VM bytecode payload.
    pub bytecode: BytecodeModule,
    /// Canonical entry module URL for graph-shaped programs.
    pub entry_url: Option<String>,
    /// Compiler metadata for every source module represented by `bytecode`.
    pub metadata: Vec<CompiledModuleMetadata>,
}

impl CompiledProgram {
    /// Build a program from one script-shaped compiler output.
    #[must_use]
    pub fn from_compiled_module(compiled: CompiledModule) -> Self {
        Self {
            bytecode: compiled.bytecode,
            entry_url: None,
            metadata: vec![compiled.metadata],
        }
    }

    /// Build a program from a linked module graph.
    #[must_use]
    pub fn from_linked_program(linked: LinkedProgram) -> Self {
        Self {
            bytecode: linked.module,
            entry_url: Some(linked.entry_url),
            metadata: linked.metadata,
        }
    }
}

impl Runtime {
    /// Compile-and-dump a file through the same script/module routing used by
    /// [`Self::run_file`](crate::Runtime::run_file).
    ///
    /// Module-shaped inputs use the runtime module loader and package graph,
    /// then return the linked bytecode plus per-source compiler metadata.
    /// Script-shaped inputs return a single-module compiled program.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn dump_file(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<CompiledProgram, OtterError> {
        let path = path.as_ref();
        let source = SourceInput::from_path(path)?;
        if source_path_has_module_extension(path) {
            return self.dump_module(path);
        }
        let package_type = {
            let loader = self.module_loader_for_entry(path);
            source_path_package_type(path, &loader)
        };
        if package_type == Some(module_loader::LoaderPackageType::Module) {
            return self.dump_module(path);
        }
        let specifier = path.to_string_lossy().to_string();
        if package_type == Some(module_loader::LoaderPackageType::CommonJs) {
            return self
                .dump(source, &specifier)
                .map(CompiledProgram::from_compiled_module);
        }
        if !source_path_has_script_extension(path) {
            let looks_like_module =
                with_program(&source.text, source.kind, program_looks_like_module).unwrap_or(false);
            if looks_like_module {
                return self.dump_module(path);
            }
        }
        self.dump(source, &specifier)
            .map(CompiledProgram::from_compiled_module)
    }

    fn dump_module(&mut self, entry_path: &std::path::Path) -> Result<CompiledProgram, OtterError> {
        let loader = self.module_loader_for_entry(entry_path);
        let linked = self
            .module_graph
            .load_program(&loader, entry_path)
            .map_err(crate::map_graph_error)?;
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        Ok(CompiledProgram::from_linked_program(linked))
    }
}
