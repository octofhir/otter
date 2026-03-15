//! Module dependency graph
//!
//! Provides topological sorting for correct module execution order.
//! Supports circular dependencies (common in npm packages like zod).

use crate::error::EngineResult;
use crate::loader::{ImportContext, ModuleLoader, ModuleType, ResolvedModule, SourceType};
use otter_vm_compiler::scan_dependencies as scan_deps_ast;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Import record tracking the relationship between modules
#[derive(Debug, Clone)]
pub struct ImportRecord {
    /// The original import specifier (e.g., "axios", "./utils", "node:fs")
    pub specifier: String,

    /// The resolved URL (e.g., "file:///node_modules/axios/dist/axios.cjs")
    pub resolved_url: Option<String>,

    /// Whether this is a require() call (true) or import statement (false)
    pub is_require: bool,

    /// Whether the imported module needs __toESM wrapper
    /// (CJS module being imported by ESM)
    pub wrap_with_to_esm: bool,

    /// Whether the imported module needs __toCommonJS wrapper
    /// (ESM module being required by CJS)
    pub wrap_with_to_commonjs: bool,
}

impl ImportRecord {
    /// Create a new import record for an ESM import
    pub fn esm_import(specifier: impl Into<String>) -> Self {
        Self {
            specifier: specifier.into(),
            resolved_url: None,
            is_require: false,
            wrap_with_to_esm: false,
            wrap_with_to_commonjs: false,
        }
    }

    /// Create a new import record for a CJS require
    pub fn cjs_require(specifier: impl Into<String>) -> Self {
        Self {
            specifier: specifier.into(),
            resolved_url: None,
            is_require: true,
            wrap_with_to_esm: false,
            wrap_with_to_commonjs: false,
        }
    }

    /// Set the resolved URL
    pub fn with_resolved_url(mut self, url: impl Into<String>) -> Self {
        self.resolved_url = Some(url.into());
        self
    }

    /// Set whether __toESM wrapper is needed
    pub fn with_to_esm(mut self, wrap: bool) -> Self {
        self.wrap_with_to_esm = wrap;
        self
    }

    /// Set whether __toCommonJS wrapper is needed
    pub fn with_to_commonjs(mut self, wrap: bool) -> Self {
        self.wrap_with_to_commonjs = wrap;
        self
    }
}

/// Module in the graph
#[derive(Debug)]
pub struct ModuleNode {
    pub module: ResolvedModule,
    /// Simple list of dependency specifiers (for backward compatibility)
    pub dependencies: Vec<String>,
    /// Detailed import records with wrapping information
    pub import_records: Vec<ImportRecord>,
    /// Compiled JavaScript (if source was TypeScript or JSON)
    pub compiled: Option<String>,
}

impl ModuleNode {
    /// Get the module type (ESM or CommonJS)
    pub fn module_type(&self) -> ModuleType {
        self.module.module_type
    }

    /// Check if this is a CommonJS module
    pub fn is_commonjs(&self) -> bool {
        self.module.module_type.is_commonjs()
    }

    /// Check if this is an ESM module
    pub fn is_esm(&self) -> bool {
        self.module.module_type.is_esm()
    }

    /// Get the dirname for CommonJS __dirname
    pub fn dirname(&self) -> Option<&str> {
        let path = self.module.url.strip_prefix("file://")?;
        std::path::Path::new(path).parent()?.to_str()
    }

    /// Get the filename for CommonJS __filename
    pub fn filename(&self) -> Option<&str> {
        self.module.url.strip_prefix("file://")
    }
}

impl ModuleNode {
    /// Get the executable source (compiled JS or original source)
    pub fn executable_source(&self) -> &str {
        self.compiled.as_deref().unwrap_or(&self.module.source)
    }
}

/// Module dependency graph
pub struct ModuleGraph {
    loader: Arc<ModuleLoader>,
    nodes: HashMap<String, ModuleNode>,
}

impl ModuleGraph {
    pub fn new(loader: Arc<ModuleLoader>) -> Self {
        Self {
            loader,
            nodes: HashMap::new(),
        }
    }

    /// Load a module and all its dependencies
    pub async fn load(&mut self, specifier: &str) -> EngineResult<()> {
        let mut visited = HashSet::new();
        let mut stack = Vec::new();

        self.load_recursive(
            specifier,
            None,
            ImportContext::ESM,
            &mut visited,
            &mut stack,
        )
        .await
    }

    async fn load_recursive(
        &mut self,
        specifier: &str,
        referrer: Option<&str>,
        context: ImportContext,
        visited: &mut HashSet<String>,
        stack: &mut Vec<String>,
    ) -> EngineResult<()> {
        // Resolve first to get canonical URL
        let resolved_url = self
            .loader
            .resolve_with_context(specifier, referrer, context)?;

        // Already loaded or in progress - skip
        // This handles both completed modules and circular dependencies
        if visited.contains(&resolved_url) || self.nodes.contains_key(&resolved_url) {
            return Ok(());
        }

        // Mark as visited immediately to prevent cycles
        visited.insert(resolved_url.clone());
        stack.push(resolved_url.clone());

        // Load the module
        let module = self
            .loader
            .load_with_context(specifier, referrer, context)
            .await?;
        let importer_type = module.module_type;
        let module_url = module.url.clone();

        // Skip parsing dependencies for built-ins (they have no source)
        let (dependencies, import_records) =
            if module.url.starts_with("otter:") || module.url.starts_with("node:") {
                (Vec::new(), Vec::new())
            } else {
                // Extract filename from URL for source type detection
                let filename = module.url.strip_prefix("file://").unwrap_or(&module.url);

                // Parse dependencies using AST-based scanner (oxc parser)
                let ast_deps = scan_deps_ast(&module.source, filename);

                let deps: Vec<String> = ast_deps.iter().map(|d| d.specifier.clone()).collect();

                // Create import records with proper context
                let records: Vec<ImportRecord> = ast_deps
                    .iter()
                    .map(|dep| {
                        let context = if dep.is_require {
                            ImportContext::CJS
                        } else {
                            ImportContext::ESM
                        };

                        let resolved = self
                            .loader
                            .resolve_with_context(&dep.specifier, Some(&module.url), context)
                            .ok();

                        let mut record = if dep.is_require {
                            ImportRecord::cjs_require(dep.specifier.clone())
                        } else {
                            ImportRecord::esm_import(dep.specifier.clone())
                        };

                        if let Some(url) = resolved {
                            record = record.with_resolved_url(url);
                        }

                        record
                    })
                    .collect();

                (deps, records)
            };

        // TypeScript is handled by the compiler (oxc TransformOptions) during compile_ext.
        // Only JSON needs wrapping here for the bundler.
        let compiled = match module.source_type {
            SourceType::Json => Some(format!("module.exports = {};", module.source)),
            _ => None,
        };

        // Add to graph BEFORE loading dependencies to handle circular deps
        // JavaScript allows circular dependencies by providing partial exports
        self.nodes.insert(
            resolved_url.clone(),
            ModuleNode {
                module,
                dependencies: dependencies.clone(),
                import_records,
                compiled,
            },
        );

        // Now recursively load dependencies
        // If any dependency tries to import us, we're already in the graph
        // Note: We continue on resolution errors to support optional dependencies
        // (packages may use try/catch around dynamic imports)
        for dep in &dependencies {
            let dep_context = if importer_type.is_commonjs() {
                ImportContext::CJS
            } else {
                ImportContext::ESM
            };
            let result =
                Box::pin(self.load_recursive(dep, Some(&module_url), dep_context, visited, stack))
                    .await;

            if let Err(_e) = result {
                // Don't fail - dependency might be optional (try/catch in source)
            }
        }

        // Update wrapping flags now that we know the imported module types
        // First, collect the module types of dependencies
        let dep_types: HashMap<String, ModuleType> = {
            let node = self.nodes.get(&resolved_url);
            if let Some(n) = node {
                n.import_records
                    .iter()
                    .filter_map(|r| {
                        r.resolved_url.as_ref().and_then(|url| {
                            self.nodes
                                .get(url)
                                .map(|dep| (url.clone(), dep.module.module_type))
                        })
                    })
                    .collect()
            } else {
                HashMap::new()
            }
        };

        // Then update the wrapping flags
        if let Some(node) = self.nodes.get_mut(&resolved_url) {
            for record in &mut node.import_records {
                if let Some(ref dep_url) = record.resolved_url
                    && let Some(&imported_type) = dep_types.get(dep_url)
                {
                    // ESM importing CJS -> needs __toESM
                    record.wrap_with_to_esm = importer_type.is_esm() && imported_type.is_commonjs();

                    // CJS requiring ESM -> needs __toCommonJS
                    record.wrap_with_to_commonjs =
                        importer_type.is_commonjs() && imported_type.is_esm();
                }
            }
        }

        stack.pop();

        Ok(())
    }

    /// Get a module by URL
    pub fn get(&self, url: &str) -> Option<&ModuleNode> {
        self.nodes.get(url)
    }

    /// Get all modules in the graph
    pub fn modules(&self) -> impl Iterator<Item = (&String, &ModuleNode)> {
        self.nodes.iter()
    }

    /// Get execution order (topological sort)
    ///
    /// Returns modules in dependency order - dependencies come before dependents.
    pub fn execution_order(&self) -> Vec<&str> {
        let mut order = Vec::new();
        let mut visited = HashSet::new();

        for specifier in self.nodes.keys() {
            self.visit_for_order(specifier, &mut visited, &mut order);
        }

        order
    }

    fn visit_for_order<'a>(
        &'a self,
        specifier: &'a str,
        visited: &mut HashSet<&'a str>,
        order: &mut Vec<&'a str>,
    ) {
        if visited.contains(specifier) {
            return;
        }

        // Mark as visited BEFORE recursing to handle circular dependencies
        visited.insert(specifier);

        if let Some(node) = self.nodes.get(specifier) {
            // First visit all dependencies
            for record in &node.import_records {
                // Prefer the resolved URL captured during graph loading.
                if let Some(dep_url) = record.resolved_url.as_deref()
                    && self.nodes.contains_key(dep_url)
                {
                    self.visit_for_order(dep_url, visited, order);
                    continue;
                }

                // Fallback: resolve now with the correct context.
                let context = if record.is_require {
                    ImportContext::CJS
                } else {
                    ImportContext::ESM
                };
                if let Ok(resolved) =
                    self.loader
                        .resolve_with_context(&record.specifier, Some(specifier), context)
                    && let Some(key) = self.nodes.keys().find(|k| k.as_str() == resolved)
                {
                    self.visit_for_order(key, visited, order);
                }
            }
        }

        order.push(specifier);
    }

    /// Check if the graph contains a module
    pub fn contains(&self, url: &str) -> bool {
        self.nodes.contains_key(url)
    }

    /// Get the number of modules in the graph
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Check if the graph is empty
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Parse import/export specifiers from source using AST-based scanning.
///
/// Uses the oxc parser for correct handling of comments, string literals,
/// and complex multi-line statements. Replaces the old regex-based parser.
pub fn parse_imports(source: &str) -> Vec<String> {
    otter_vm_compiler::scan_specifiers(source, "input.js")
}

/// Parse require() specifiers from CommonJS source using AST-based scanning.
pub fn parse_requires(source: &str) -> Vec<String> {
    otter_vm_compiler::scan_dependencies(source, "input.cjs")
        .into_iter()
        .filter(|d| d.is_require)
        .map(|d| d.specifier)
        .collect()
}

/// Parse dependencies from source based on module type using AST-based scanning.
pub fn parse_dependencies(source: &str, module_type: ModuleType) -> Vec<String> {
    let filename = match module_type {
        ModuleType::ESM => "input.mjs",
        ModuleType::CommonJS => "input.cjs",
    };
    otter_vm_compiler::scan_specifiers(source, filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_imports_static() {
        let source = r#"
            import { foo } from './foo.js';
            import bar from "https://esm.sh/bar";
            import * as utils from './utils.js';
        "#;

        let imports = parse_imports(source);
        assert_eq!(imports.len(), 3);
        assert!(imports.contains(&"./foo.js".to_string()));
        assert!(imports.contains(&"https://esm.sh/bar".to_string()));
        assert!(imports.contains(&"./utils.js".to_string()));
    }

    #[test]
    fn test_parse_imports_dynamic() {
        let source = r#"
            const mod = await import('./dynamic.js');
            import("./another.js").then(m => m.default);
        "#;

        let imports = parse_imports(source);
        assert_eq!(imports.len(), 2);
        assert!(imports.contains(&"./dynamic.js".to_string()));
        assert!(imports.contains(&"./another.js".to_string()));
    }

    #[test]
    fn test_parse_imports_export_from() {
        let source = r#"
            export { foo } from './foo.js';
            export * from './all.js';
        "#;

        let imports = parse_imports(source);
        assert_eq!(imports.len(), 2);
        assert!(imports.contains(&"./foo.js".to_string()));
        assert!(imports.contains(&"./all.js".to_string()));
    }

    #[test]
    fn test_parse_imports_side_effect() {
        let source = r#"
            import './side-effect.js';
            import "https://esm.sh/polyfill";
        "#;

        let imports = parse_imports(source);
        assert_eq!(imports.len(), 2);
        assert!(imports.contains(&"./side-effect.js".to_string()));
        assert!(imports.contains(&"https://esm.sh/polyfill".to_string()));
    }

    #[test]
    fn test_parse_imports_no_duplicates() {
        let source = r#"
            import { foo } from './mod.js';
            import { bar } from './mod.js';
            const x = await import('./mod.js');
        "#;

        let imports = parse_imports(source);
        assert_eq!(imports.len(), 1);
        assert!(imports.contains(&"./mod.js".to_string()));
    }

    #[test]
    fn test_parse_imports_mixed() {
        let source = r#"
            import { foo } from './foo.js';
            import bar from "https://esm.sh/bar";
            const dynamic = await import('./dynamic.js');
            export { baz } from './baz.js';
        "#;

        let imports = parse_imports(source);
        assert_eq!(imports.len(), 4);
        assert!(imports.contains(&"./foo.js".to_string()));
        assert!(imports.contains(&"https://esm.sh/bar".to_string()));
        assert!(imports.contains(&"./dynamic.js".to_string()));
        assert!(imports.contains(&"./baz.js".to_string()));
    }

    #[test]
    fn test_parse_requires_basic() {
        let source = r#"
            const fs = require('fs');
            const path = require("path");
            const lib = require('./lib.cjs');
        "#;

        let requires = parse_requires(source);
        assert_eq!(requires.len(), 3);
        assert!(requires.contains(&"fs".to_string()));
        assert!(requires.contains(&"path".to_string()));
        assert!(requires.contains(&"./lib.cjs".to_string()));
    }

    #[test]
    fn test_parse_requires_inline() {
        let source = r#"
            console.log(require('./config.json').version);
            const { helper } = require('./utils');
        "#;

        let requires = parse_requires(source);
        assert_eq!(requires.len(), 2);
        assert!(requires.contains(&"./config.json".to_string()));
        assert!(requires.contains(&"./utils".to_string()));
    }

    #[test]
    fn test_parse_requires_no_duplicates() {
        let source = r#"
            const fs1 = require('fs');
            const fs2 = require('fs');
            require('fs');
        "#;

        let requires = parse_requires(source);
        assert_eq!(requires.len(), 1);
        assert!(requires.contains(&"fs".to_string()));
    }

    #[test]
    fn test_parse_requires_scoped_packages() {
        let source = r#"
            const pkg = require('@scope/package');
            const sub = require('@org/lib/subpath');
        "#;

        let requires = parse_requires(source);
        assert_eq!(requires.len(), 2);
        assert!(requires.contains(&"@scope/package".to_string()));
        assert!(requires.contains(&"@org/lib/subpath".to_string()));
    }

    #[test]
    fn test_parse_dependencies_esm() {
        let source = "import foo from './foo.js';";
        let deps = parse_dependencies(source, ModuleType::ESM);
        assert_eq!(deps.len(), 1);
        assert!(deps.contains(&"./foo.js".to_string()));
    }

    #[test]
    fn test_parse_dependencies_commonjs() {
        let source = "const foo = require('./foo.cjs');";
        let deps = parse_dependencies(source, ModuleType::CommonJS);
        assert_eq!(deps.len(), 1);
        assert!(deps.contains(&"./foo.cjs".to_string()));
    }
}
