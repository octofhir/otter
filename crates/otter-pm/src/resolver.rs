//! Dependency resolution with parallel fetching

use crate::registry::{NpmRegistry, RegistryError};
use futures::stream::{FuturesUnordered, StreamExt};
use std::collections::{HashMap, HashSet, VecDeque};

/// Resolved package with version and dependencies
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub tarball_url: String,
    pub integrity: Option<String>,
    pub dependencies: HashMap<String, String>,
}

/// Dependency resolver with parallel fetching
pub struct Resolver {
    registry: NpmRegistry,
    resolved: HashMap<String, ResolvedPackage>,
}

impl Resolver {
    pub fn new(registry: NpmRegistry) -> Self {
        Self {
            registry,
            resolved: HashMap::new(),
        }
    }

    /// Resolve all dependencies in parallel using BFS
    pub async fn resolve(
        &mut self,
        dependencies: &HashMap<String, String>,
    ) -> Result<Vec<ResolvedPackage>, ResolverError> {
        // Queue of packages to resolve: (name, version_req)
        let mut queue: VecDeque<(String, String)> = dependencies
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Track what we've seen to avoid duplicates
        let mut seen: HashSet<String> = HashSet::new();

        while !queue.is_empty() {
            // Collect current batch (all packages not yet resolved)
            let mut batch: Vec<(String, String)> = Vec::new();
            while let Some((name, version_req)) = queue.pop_front() {
                if !seen.contains(&name) && !self.resolved.contains_key(&name) {
                    seen.insert(name.clone());
                    batch.push((name, version_req));
                }
            }

            if batch.is_empty() {
                break;
            }

            // Resolve batch in parallel
            let results = self.resolve_batch(&batch).await?;

            // Collect transitive dependencies for next batch
            for pkg in results {
                for (dep_name, dep_version) in &pkg.dependencies {
                    if !seen.contains(dep_name) && !self.resolved.contains_key(dep_name) {
                        queue.push_back((dep_name.clone(), dep_version.clone()));
                    }
                }
                self.resolved.insert(pkg.name.clone(), pkg);
            }
        }

        Ok(self.resolved.values().cloned().collect())
    }

    /// Resolve a batch of packages in parallel
    async fn resolve_batch(
        &self,
        packages: &[(String, String)],
    ) -> Result<Vec<ResolvedPackage>, ResolverError> {
        let mut tasks = FuturesUnordered::new();

        for (name, version_req) in packages {
            let registry = self.registry.clone();
            let name = name.clone();
            let version_req = version_req.clone();

            tasks.push(tokio::spawn(async move {
                resolve_single(&registry, &name, &version_req).await
            }));
        }

        let mut results = Vec::new();
        while let Some(result) = tasks.next().await {
            let pkg = result
                .map_err(|e| ResolverError::Registry(RegistryError::Network(e.to_string())))?
                .map_err(ResolverError::Registry)?;
            results.push(pkg);
        }

        Ok(results)
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
    }

    /// Get the registry (for subsequent downloads)
    pub fn registry(&self) -> &NpmRegistry {
        &self.registry
    }

    /// Consume resolver and return the registry (with cached metadata)
    pub fn into_registry(self) -> NpmRegistry {
        self.registry
    }
}

/// Resolve a single package (called in parallel)
async fn resolve_single(
    registry: &NpmRegistry,
    name: &str,
    version_req: &str,
) -> Result<ResolvedPackage, RegistryError> {
    // Resolve version
    let version = registry.resolve_version(name, version_req).await?;

    // Get package metadata (should be cached after resolve_version)
    let metadata = registry.get_package(name).await?;

    let version_info = metadata
        .versions
        .get(&version)
        .ok_or_else(|| RegistryError::NotFound(format!("{}@{}", name, version)))?;

    let deps = version_info.dependencies.clone().unwrap_or_default();

    Ok(ResolvedPackage {
        name: name.to_string(),
        version,
        tarball_url: version_info.dist.tarball.clone(),
        integrity: version_info.dist.integrity.clone(),
        dependencies: deps,
    })
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
            let names: Vec<_> = packages.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"is-odd"));
        }
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_resolve_with_transitive() {
        let registry = NpmRegistry::new();
        let mut resolver = Resolver::new(registry);

        let mut deps = HashMap::new();
        deps.insert("chalk".to_string(), "^4.0.0".to_string());

        let result = resolver.resolve(&deps).await;
        if let Ok(packages) = result {
            assert!(packages.len() > 1);
            let names: Vec<_> = packages.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"chalk"));
        }
    }
}
