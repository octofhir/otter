//! Exact classic-script text admission before bytecode publication.
//!
//! # Contents
//! - `Runtime::prepare_script_source` stamps absent defining URLs and returns
//!   the bytecode with its immutable source registry.
//! - `script_sources` builds the same registry for verified bootstrap bodies.
//!
//! # Invariants
//! Every linked body carries its defining text; reusing a URL cannot change an
//! earlier body. Source and line-index leases are admitted before publication.
//! Compile-only operations do not retain a source registry. Existing nonempty
//! function URLs remain authoritative.
//!
//! # See also
//! - `crate::realm` for additional-realm classic scripts.
//! - `otter_vm::source_registry` for the sole immutable source owner.

use crate::{Runtime, SourceInput, error::OtterError, module_loader};
use otter_bytecode::BytecodeModule;
use otter_vm::source_registry::SourceRegistry;
use std::collections::BTreeMap;

pub(crate) fn script_sources(
    module: &BytecodeModule,
    text: otter_resource::SharedSource,
    specifier: &str,
    account: &otter_resource::ResourceAccount,
) -> Result<SourceRegistry, OtterError> {
    let mut sources = BTreeMap::new();
    sources.insert(specifier.to_owned(), text.clone());
    if !module.module.is_empty() {
        sources.insert(module.module.clone(), text);
    }
    SourceRegistry::new(sources, account).map_err(OtterError::from)
}

impl Runtime {
    pub(crate) fn prepare_script_source(
        &mut self,
        mut module: BytecodeModule,
        source: SourceInput,
        specifier: &str,
    ) -> Result<(BytecodeModule, SourceRegistry), OtterError> {
        for function in &mut module.functions {
            if function.module_url.is_empty() {
                function.module_url = specifier.to_owned();
            }
        }
        let text =
            module_loader::admit_source(&self.config.resource_account, specifier, source.text)
                .map_err(module_loader::LoaderError::into_otter_error)?;
        let sources = script_sources(&module, text, specifier, &self.config.resource_account)?;
        Ok((module, sources))
    }
}
