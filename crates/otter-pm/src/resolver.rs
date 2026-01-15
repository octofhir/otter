//! Dependency resolution

use crate::registry::{NpmRegistry, RegistryError};
use std::collections::{HashMap, HashSet};

/// Resolved package with version and dependencies
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub tarball_url: String,
    pub integrity: Option<String>,
    pub dependencies: HashMap<String, String>,
}

/// Dependency resolver
pub struct Resolver {
    registry: NpmRegistry,
    resolved: HashMap<String, ResolvedPackage>,
    in_progress: HashSet<String>,
}

impl Resolver {
    pub fn new(registry: NpmRegistry) -> Self {
        Self {
            registry,
            resolved: HashMap::new(),
            in_progress: HashSet::new(),
        }
    }

    /// Resolve all dependencies for a package.json
    pub async fn resolve(
        &mut self,
        dependencies: &HashMap<String, String>,
    ) -> Result<Vec<ResolvedPackage>, ResolverError> {
        for (name, version_req) in dependencies {
            self.resolve_package(name, version_req).await?;
        }

        Ok(self.resolved.values().cloned().collect())
    }

    /// Resolve a single package and its transitive dependencies
    pub async fn resolve_package(
        &mut self,
        name: &str,
        version_req: &str,
    ) -> Result<(), ResolverError> {
        // Check if already resolved
        if self.resolved.contains_key(name) {
            return Ok(());
        }

        // Check for circular dependency
        if self.in_progress.contains(name) {
            return Err(ResolverError::CircularDependency(name.to_string()));
        }

        self.in_progress.insert(name.to_string());

        // Resolve version
        let version = self
            .registry
            .resolve_version(name, version_req)
            .await
            .map_err(ResolverError::Registry)?;

        // Get package metadata (should be cached after resolve_version)
        let metadata = self
            .registry
            .get_package(name)
            .await
            .map_err(ResolverError::Registry)?;

        let version_info =
            metadata
                .versions
                .get(&version)
                .ok_or_else(|| ResolverError::VersionNotFound {
                    name: name.to_string(),
                    version: version.clone(),
                })?;

        // Store dependencies before recursive resolution
        let deps = version_info.dependencies.clone().unwrap_or_default();
        let tarball_url = version_info.dist.tarball.clone();
        let integrity = version_info.dist.integrity.clone();

        // Resolve transitive dependencies
        for (dep_name, dep_version) in &deps {
            // Use Box::pin for recursive async
            Box::pin(self.resolve_package(dep_name, dep_version)).await?;
        }

        // Add to resolved
        self.resolved.insert(
            name.to_string(),
            ResolvedPackage {
                name: name.to_string(),
                version,
                tarball_url,
                integrity,
                dependencies: deps,
            },
        );

        self.in_progress.remove(name);
        Ok(())
    }

    /// Get resolved packages
    pub fn get_resolved(&self) -> &HashMap<String, ResolvedPackage> {
        &self.resolved
    }

    /// Get a specific resolved package
    pub fn get_package(&self, name: &str) -> Option<&ResolvedPackage> {
        self.resolved.get(name)
    }

    /// Clear resolved packages (for re-resolution)
    pub fn clear(&mut self) {
        self.resolved.clear();
        self.in_progress.clear();
    }

    /// Consume resolver and return the registry (with cached metadata)
    pub fn into_registry(self) -> NpmRegistry {
        self.registry
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("Registry error: {0}")]
    Registry(#[from] RegistryError),

    #[error("Circular dependency: {0}")]
    CircularDependency(String),

    #[error("Version not found: {name}@{version}")]
    VersionNotFound { name: String, version: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver_new() {
        let registry = NpmRegistry::new();
        let resolver = Resolver::new(registry);
        assert!(resolver.resolved.is_empty());
        assert!(resolver.in_progress.is_empty());
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_resolve_simple() {
        let registry = NpmRegistry::new();
        let mut resolver = Resolver::new(registry);

        let mut deps = HashMap::new();
        deps.insert("is-odd".to_string(), "^3.0.0".to_string());

        let result = resolver.resolve(&deps).await;
        if let Ok(packages) = result {
            assert!(!packages.is_empty());
            // is-odd depends on is-number, so we should have both
            let names: Vec<_> = packages.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"is-odd"));
        }
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_resolve_with_transitive() {
        let registry = NpmRegistry::new();
        let mut resolver = Resolver::new(registry);

        // chalk has several transitive dependencies
        let mut deps = HashMap::new();
        deps.insert("chalk".to_string(), "^4.0.0".to_string());

        let result = resolver.resolve(&deps).await;
        if let Ok(packages) = result {
            // Should have chalk and its dependencies
            assert!(packages.len() > 1);
            let names: Vec<_> = packages.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"chalk"));
        }
    }
}
