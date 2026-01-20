//! Dynamic import support for runtime module loading.
//!
//! This module provides the `__otter_load_module` native op that enables
//! true runtime dynamic imports (`import()`) for npm packages and local modules.
//!
//! # How it works
//!
//! When JavaScript calls `import(specifier)`, the bootstrap code checks:
//! 1. Pre-bundled ESM modules in `__otter_modules`
//! 2. Pre-bundled CJS modules in `__otter_cjs_modules`
//! 3. Node.js builtins (`node:*`)
//! 4. Otter builtins (`otter:*`)
//! 5. **Falls back to `__otter_load_module`** (this op)
//!
//! This op resolves the specifier, loads all dependencies, bundles them,
//! and returns code that the JS side can eval to register the modules.

use crate::graph::ModuleGraph;
use crate::loader::{LoaderConfig, ModuleLoader};
use otter_runtime::extension::{op_async, Extension};
use otter_runtime::modules::{ModuleFormat, ModuleInfo, bundle_modules_mixed};
use otter_runtime::JscError;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// Create the dynamic import extension.
///
/// The extension provides a single async op `__otter_load_module` that:
/// 1. Resolves the specifier using `ModuleLoader`
/// 2. Loads and transpiles the module and its dependencies
/// 3. Bundles them with `bundle_modules_mixed`
/// 4. Returns bundled code for JS to eval
///
/// # Arguments
///
/// * `loader_config` - Configuration for the module loader (base_dir, allowlists, etc.)
pub fn extension(loader_config: LoaderConfig) -> Extension {
    let loader = Arc::new(ModuleLoader::new(loader_config));

    Extension::new("dynamic_import").with_ops(vec![op_async("__otter_load_module", {
        let loader = loader.clone();
        move |_ctx, args| {
            let loader = loader.clone();
            async move {
                let specifier = args
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| JscError::internal("specifier required".to_string()))?;

                // Get optional referrer from second argument
                let referrer = args.get(1).and_then(|v| v.as_str());

                load_module_async(&loader, specifier, referrer).await
            }
        }
    })])
}

/// Load a module and its dependencies at runtime.
///
/// Returns a JSON object with:
/// - `code`: The bundled JavaScript code to eval
/// - `entry`: The resolved entry URL for lookup in `__otter_modules`
/// - `urls`: List of all module URLs in execution order
async fn load_module_async(
    loader: &ModuleLoader,
    specifier: &str,
    referrer: Option<&str>,
) -> Result<Value, JscError> {
    // Resolve specifier to get the canonical URL
    let resolved_url = loader.resolve(specifier, referrer)?;

    // Create module graph and load dependencies
    let mut graph = ModuleGraph::new(Arc::new(ModuleLoader::new(loader.config().clone())));

    // Load the module and all its dependencies
    graph
        .load(&resolved_url)
        .await
        .map_err(|e| JscError::ModuleError(e.to_string()))?;

    // Get execution order
    let order: Vec<String> = graph
        .execution_order()
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Build module infos for bundling
    let mut modules_data: Vec<(
        String,
        String,
        HashMap<String, String>,
        bool,
        Option<String>,
        Option<String>,
    )> = Vec::new();

    for url in &order {
        // Skip builtins - they have no source and are resolved via runtime lookups
        if url.starts_with("node:") || url.starts_with("otter:") {
            continue;
        }

        if let Some(node) = graph.get(url) {
            // Build dependency map for this module
            let mut deps = HashMap::new();
            for record in &node.import_records {
                if let Some(resolved) = &record.resolved_url {
                    deps.insert(record.specifier.clone(), resolved.clone());
                }
            }

            modules_data.push((
                url.clone(),
                node.executable_source().to_string(),
                deps,
                node.is_commonjs(),
                node.dirname().map(String::from),
                node.filename().map(String::from),
            ));
        }
    }

    // Convert to ModuleInfo for mixed bundling
    let module_infos: Vec<ModuleInfo<'_>> = modules_data
        .iter()
        .map(|(url, src, deps, is_cjs, dirname, filename)| ModuleInfo {
            url: url.as_str(),
            source: src.as_str(),
            dependencies: deps,
            format: if *is_cjs {
                ModuleFormat::CommonJS
            } else {
                ModuleFormat::ESM
            },
            dirname: dirname.as_deref(),
            filename: filename.as_deref(),
        })
        .collect();

    // Bundle all modules
    let bundle = bundle_modules_mixed(module_infos);

    // Return the bundle + metadata
    // The JS side will:
    // 1. eval(result.code) to register modules in __otter_modules/__otter_cjs_modules
    // 2. Return __otter_modules[result.entry]
    Ok(json!({
        "code": bundle,
        "entry": resolved_url,
        "urls": order,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let config = LoaderConfig::default();
        let ext = extension(config);
        assert_eq!(ext.name(), "dynamic_import");
    }
}
