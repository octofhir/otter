//! Module dependency graph with cycle detection
//!
//! Provides topological sorting for correct module execution order
//! and detects circular dependencies.

use crate::loader::{ModuleLoader, ResolvedModule, SourceType};
use otter_runtime::{JscError, JscResult, transpile_typescript};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Module in the graph
#[derive(Debug)]
pub struct ModuleNode {
    pub module: ResolvedModule,
    pub dependencies: Vec<String>,
    /// Compiled JavaScript (if source was TypeScript)
    pub compiled: Option<String>,
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
    pub async fn load(&mut self, specifier: &str) -> JscResult<()> {
        let mut visited = HashSet::new();
        let mut stack = Vec::new();

        self.load_recursive(specifier, None, &mut visited, &mut stack)
            .await
    }

    async fn load_recursive(
        &mut self,
        specifier: &str,
        referrer: Option<&str>,
        visited: &mut HashSet<String>,
        stack: &mut Vec<String>,
    ) -> JscResult<()> {
        // Resolve first to get canonical URL
        let resolved_url = self.loader.resolve(specifier, referrer)?;

        // Check for cycles using resolved URL
        if stack.contains(&resolved_url) {
            return Err(JscError::ModuleError(format!(
                "Circular dependency detected: {} -> {}",
                stack.join(" -> "),
                resolved_url
            )));
        }

        // Already loaded
        if visited.contains(&resolved_url) {
            return Ok(());
        }

        stack.push(resolved_url.clone());

        // Load the module
        let module = self.loader.load(specifier, referrer).await?;

        // Skip parsing dependencies for node: built-ins (they have no source)
        let dependencies = if module.url.starts_with("node:") {
            Vec::new()
        } else {
            parse_imports(&module.source)
        };

        // Recursively load dependencies
        for dep in &dependencies {
            Box::pin(self.load_recursive(dep, Some(&module.url), visited, stack)).await?;
        }

        // Compile TypeScript if needed
        let compiled = if module.source_type == SourceType::TypeScript {
            let result = transpile_typescript(&module.source).map_err(|e| {
                JscError::ModuleError(format!("Failed to transpile '{}': {}", module.url, e))
            })?;
            Some(result.code)
        } else {
            None
        };

        // Add to graph using resolved URL as key
        self.nodes.insert(
            resolved_url.clone(),
            ModuleNode {
                module,
                dependencies,
                compiled,
            },
        );

        visited.insert(resolved_url.clone());
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

        if let Some(node) = self.nodes.get(specifier) {
            // First visit all dependencies
            for dep in &node.dependencies {
                // Resolve the dependency to its canonical URL
                if let Ok(resolved) = self.loader.resolve(dep, Some(specifier))
                    && let Some(key) = self.nodes.keys().find(|k| **k == resolved) {
                        self.visit_for_order(key, visited, order);
                    }
            }
        }

        visited.insert(specifier);
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

/// Parse import statements from source
///
/// Extracts both static and dynamic imports.
pub fn parse_imports(source: &str) -> Vec<String> {
    let mut imports = Vec::new();

    // Static imports: import ... from '...'
    // Handles: import foo from 'x', import { foo } from 'x', import * as foo from 'x'
    let import_re = Regex::new(r#"(?m)^\s*import\s+(?:.*?\s+from\s+)?['"]([^'"]+)['"]"#).unwrap();

    // Dynamic imports: import('...')
    let dynamic_re = Regex::new(r#"import\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap();

    // Export from: export ... from '...'
    let export_re = Regex::new(r#"(?m)^\s*export\s+.*?\s+from\s+['"]([^'"]+)['"]"#).unwrap();

    for cap in import_re.captures_iter(source) {
        let specifier = cap[1].to_string();
        if !imports.contains(&specifier) {
            imports.push(specifier);
        }
    }

    for cap in dynamic_re.captures_iter(source) {
        let specifier = cap[1].to_string();
        if !imports.contains(&specifier) {
            imports.push(specifier);
        }
    }

    for cap in export_re.captures_iter(source) {
        let specifier = cap[1].to_string();
        if !imports.contains(&specifier) {
            imports.push(specifier);
        }
    }

    imports
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
}
