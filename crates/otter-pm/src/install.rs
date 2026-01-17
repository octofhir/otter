//! Package installation

use crate::binary_lockfile::{BinaryLockEntry, BinaryLockfile};
use crate::content_store::{ContentStore, PackageIndex};
use crate::lockfile::{Lockfile, LockfileEntry};
use crate::progress::InstallProgress;
use crate::registry::NpmRegistry;
use crate::resolver::{ResolvedPackage, Resolver};
use crate::types::install_bundled_types;
use futures::stream::{FuturesUnordered, StreamExt};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Maximum number of concurrent downloads
const MAX_CONCURRENT_DOWNLOADS: usize = 32;

/// Package installer
pub struct Installer {
    registry: NpmRegistry,
    node_modules: PathBuf,
    cache_dir: PathBuf,
    content_store: ContentStore,
    progress: InstallProgress,
    progress_enabled: bool,
}

impl Installer {
    pub fn new(project_dir: &Path) -> Self {
        let progress_enabled = std::io::stderr().is_terminal();

        Self {
            registry: NpmRegistry::new(),
            node_modules: project_dir.join("node_modules"),
            cache_dir: dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("otter/packages"),
            content_store: ContentStore::new(),
            // Default to silent for the fast-paths; enable progress only for the slow (network) path.
            progress: InstallProgress::silent(),
            progress_enabled,
        }
    }

    /// Create installer with silent progress (for tests)
    pub fn new_silent(project_dir: &Path) -> Self {
        Self {
            registry: NpmRegistry::new(),
            node_modules: project_dir.join("node_modules"),
            cache_dir: dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("otter/packages"),
            content_store: ContentStore::new(),
            progress: InstallProgress::silent(),
            progress_enabled: false,
        }
    }

    fn install_bundled_types_if_needed(&self) -> Result<(), InstallError> {
        install_bundled_types(&self.node_modules).map_err(|e| InstallError::Io(e.to_string()))
    }

    /// Try frozen install from binary lockfile (fastest path - no parsing overhead)
    /// Returns Some(lockfile) if successful, None if fallback needed
    fn try_binary_frozen_install(
        &mut self,
        binary_lock: &BinaryLockfile,
        deps: &HashMap<String, String>,
    ) -> Result<Option<Lockfile>, InstallError> {
        // Check if all requested deps are in lockfile
        for dep_name in deps.keys() {
            if !binary_lock.packages.contains_key(dep_name) {
                return Ok(None); // New dependency, need full resolve
            }
        }

        // Fast check: if node_modules doesn't exist or is empty, install all
        let node_modules_empty = !self.node_modules.exists()
            || fs::read_dir(&self.node_modules)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true);

        // Check if all locked packages are in CAS
        let mut packages_to_install: Vec<(String, PackageIndex)> = Vec::new();
        for (name, entry) in &binary_lock.packages {
            let Some(index) = self.content_store.get_package_index(name, &entry.version) else {
                return Ok(None); // Package not in CAS, need download
            };

            if !node_modules_empty {
                // Check if already installed with correct version
                let pkg_json = self.node_modules.join(name).join("package.json");
                if pkg_json.exists() {
                    if let Ok(content) = fs::read_to_string(&pkg_json) {
                        if let Ok(existing) = serde_json::from_str::<PackageJson>(&content) {
                            if existing.version.as_deref() == Some(&entry.version) {
                                continue; // Already installed
                            }
                        }
                    }
                }
            }

            packages_to_install.push((name.clone(), index));
        }

        // Convert to regular lockfile for return
        let mut lockfile = Lockfile::new();
        for (name, entry) in &binary_lock.packages {
            lockfile.packages.insert(
                name.clone(),
                LockfileEntry {
                    version: entry.version.clone(),
                    resolved: entry.resolved.clone(),
                    integrity: entry.integrity.clone(),
                    dependencies: HashMap::new(),
                },
            );
        }

        if packages_to_install.is_empty() {
            // Everything already installed
            self.install_bundled_types_if_needed()?;
            self.progress.finish(binary_lock.len());
            return Ok(Some(lockfile));
        }

        // Fast path: install all from CAS in parallel using rayon
        let store = &self.content_store;
        let node_modules = &self.node_modules;

        let results: Vec<Result<(), String>> = packages_to_install
            .par_iter()
            .map(|(name, index)| {
                let dest = node_modules.join(name);
                store
                    .install_from_index(index, &dest)
                    .map_err(|e| format!("{}: {}", name, e))
            })
            .collect();

        // Check for errors
        for result in results {
            result.map_err(InstallError::Io)?;
        }

        self.install_bundled_types_if_needed()?;

        self.progress.finish(binary_lock.len());
        Ok(Some(lockfile))
    }

    /// Try frozen install from JSON lockfile (fallback)
    /// Returns Some(lockfile) if successful, None if fallback to full install needed
    async fn try_frozen_install(
        &mut self,
        lockfile: &Lockfile,
        deps: &HashMap<String, String>,
    ) -> Result<Option<Lockfile>, InstallError> {
        // Check if all requested deps are in lockfile
        for dep_name in deps.keys() {
            if !lockfile.packages.contains_key(dep_name) {
                return Ok(None); // New dependency, need full resolve
            }
        }

        // Fast check: if node_modules doesn't exist or is empty, install all
        let node_modules_empty = !self.node_modules.exists()
            || fs::read_dir(&self.node_modules)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true);

        // Check if all locked packages are in CAS
        let mut packages_to_install: Vec<(String, PackageIndex)> = Vec::new();
        for (name, entry) in &lockfile.packages {
            let Some(index) = self.content_store.get_package_index(name, &entry.version) else {
                return Ok(None); // Package not in CAS, need download
            };

            if !node_modules_empty {
                // Check if already installed with correct version
                let pkg_json = self.node_modules.join(name).join("package.json");
                if pkg_json.exists() {
                    if let Ok(content) = fs::read_to_string(&pkg_json) {
                        if let Ok(existing) = serde_json::from_str::<PackageJson>(&content) {
                            if existing.version.as_deref() == Some(&entry.version) {
                                continue; // Already installed
                            }
                        }
                    }
                }
            }

            packages_to_install.push((name.clone(), index));
        }

        if packages_to_install.is_empty() {
            // Everything already installed
            self.install_bundled_types_if_needed()?;
            self.progress.finish(lockfile.packages.len());
            return Ok(Some(lockfile.clone()));
        }

        // Fast path: install all from CAS using hardlinks
        self.progress.start_install(packages_to_install.len());

        for (name, index) in &packages_to_install {
            let dest = self.node_modules.join(name);
            self.content_store
                .install_from_index(index, &dest)
                .map_err(|e| InstallError::Io(e.to_string()))?;
            self.progress.tick_install(name);
        }

        self.progress.finish_install();

        self.install_bundled_types_if_needed()?;

        self.progress.finish(lockfile.packages.len());
        Ok(Some(lockfile.clone()))
    }

    /// Install dependencies from package.json
    pub async fn install(&mut self, package_json: &Path) -> Result<Lockfile, InstallError> {
        // Read package.json
        let content =
            fs::read_to_string(package_json).map_err(|e| InstallError::Io(e.to_string()))?;

        let pkg: PackageJson =
            serde_json::from_str(&content).map_err(|e| InstallError::Parse(e.to_string()))?;

        // Collect all dependencies
        let mut deps = pkg.dependencies.unwrap_or_default();
        if let Some(dev_deps) = pkg.dev_dependencies {
            deps.extend(dev_deps);
        }

        // Ensure node_modules exists for bundled types
        fs::create_dir_all(&self.node_modules).map_err(|e| InstallError::Io(e.to_string()))?;

        if deps.is_empty() {
            self.progress.finish(0);
            return Ok(Lockfile::new());
        }

        // Try frozen install from binary lockfile first (fastest path)
        let binary_lockfile_path = package_json.parent().unwrap().join("otter.lockb");
        if let Ok(binary_lock) = BinaryLockfile::load(&binary_lockfile_path) {
            if let Some(result) = self.try_binary_frozen_install(&binary_lock, &deps)? {
                return Ok(result);
            }
        }

        // Try JSON lockfile as fallback
        let lockfile_path = package_json.parent().unwrap().join("otter.lock");
        if let Ok(lockfile) = Lockfile::load(&lockfile_path) {
            if let Some(result) = self.try_frozen_install(&lockfile, &deps).await? {
                return Ok(result);
            }
        }

        // We are about to do network work; enable interactive progress for this path only.
        if self.progress_enabled {
            self.progress = InstallProgress::new();
        }

        // Start resolve phase
        self.progress.start_resolve(deps.len());

        // Resolve dependencies
        let mut resolver = Resolver::new(self.registry.clone());
        let resolved = resolver
            .resolve(&deps)
            .await
            .map_err(|e| InstallError::Resolve(e.to_string()))?;

        self.progress.finish_resolve(resolved.len());

        let mut lockfile = Lockfile::new();

        if resolved.is_empty() {
            self.progress.finish(0);
            return Ok(lockfile);
        }

        // Filter packages that need installation
        let packages_to_install: Vec<_> = resolved
            .iter()
            .filter(|pkg| !self.is_installed(pkg))
            .cloned()
            .collect();

        let total_packages = resolved.len();

        // Split into: packages in CAS store vs packages that need download
        let mut in_store: Vec<(ResolvedPackage, PackageIndex)> = Vec::new();
        let mut need_download: Vec<ResolvedPackage> = Vec::new();

        for pkg in packages_to_install {
            if let Some(index) = self.content_store.get_package_index(&pkg.name, &pkg.version) {
                in_store.push((pkg, index));
            } else {
                need_download.push(pkg);
            }
        }

        // Fast path: Install from store using hardlinks
        if !in_store.is_empty() {
            self.progress.start_install(in_store.len());
            for (pkg, index) in &in_store {
                let dest = self.node_modules.join(&pkg.name);
                self.content_store
                    .install_from_index(index, &dest)
                    .map_err(|e| InstallError::Io(e.to_string()))?;

                self.progress.tick_install(&pkg.name);

                lockfile.packages.insert(
                    pkg.name.clone(),
                    LockfileEntry {
                        version: pkg.version.clone(),
                        resolved: pkg.tarball_url.clone(),
                        integrity: pkg.integrity.clone(),
                        dependencies: pkg.dependencies.clone(),
                    },
                );
            }
            self.progress.finish_install();
        }

        // Slow path: Download packages that aren't in store
        if !need_download.is_empty() {
            self.progress.start_download(need_download.len());

            let download_results = self
                .download_packages_parallel(need_download, resolver.registry())
                .await?;

            self.progress.finish_download();

            // Store in CAS and install via hardlinks
            self.progress.start_install(download_results.len());

            let mut store_tasks = FuturesUnordered::new();
            let store = self.content_store.clone();
            let node_modules = self.node_modules.clone();

            for (pkg, tarball) in download_results {
                let store = store.clone();
                let dest = node_modules.join(&pkg.name);
                let pkg_clone = pkg.clone();

                store_tasks.push(tokio::spawn(async move {
                    tokio::task::spawn_blocking(move || {
                        // Store in CAS
                        let index =
                            store.store_package_from_tarball(&pkg_clone.name, &pkg_clone.version, &tarball)?;
                        // Install via hardlinks
                        store.install_from_index(&index, &dest)?;
                        Ok::<_, std::io::Error>(pkg_clone)
                    })
                    .await
                    .map_err(|e| InstallError::Io(e.to_string()))?
                    .map_err(|e| InstallError::Io(e.to_string()))
                }));
            }

            while let Some(result) = store_tasks.next().await {
                let pkg = result
                    .map_err(|e| InstallError::Io(e.to_string()))??;

                self.progress.tick_install(&pkg.name);

                lockfile.packages.insert(
                    pkg.name.clone(),
                    LockfileEntry {
                        version: pkg.version.clone(),
                        resolved: pkg.tarball_url.clone(),
                        integrity: pkg.integrity.clone(),
                        dependencies: pkg.dependencies.clone(),
                    },
                );
            }

            self.progress.finish_install();
        }

        // Add already-installed packages to lockfile
        for pkg in &resolved {
            if !lockfile.packages.contains_key(&pkg.name) {
                lockfile.packages.insert(
                    pkg.name.clone(),
                    LockfileEntry {
                        version: pkg.version.clone(),
                        resolved: pkg.tarball_url.clone(),
                        integrity: pkg.integrity.clone(),
                        dependencies: pkg.dependencies.clone(),
                    },
                );
            }
        }

        self.install_bundled_types_if_needed()?;

        // Write JSON lockfile
        let lockfile_path = package_json.parent().unwrap().join("otter.lock");
        lockfile
            .save(&lockfile_path)
            .map_err(|e| InstallError::Io(e.to_string()))?;

        // Write binary lockfile (faster for subsequent installs)
        let binary_lockfile_path = package_json.parent().unwrap().join("otter.lockb");
        let mut binary_lock = BinaryLockfile::new();
        for (name, entry) in &lockfile.packages {
            binary_lock.packages.insert(
                name.clone(),
                BinaryLockEntry {
                    version: entry.version.clone(),
                    resolved: entry.resolved.clone(),
                    integrity: entry.integrity.clone(),
                },
            );
        }
        binary_lock
            .save(&binary_lockfile_path)
            .map_err(|e| InstallError::Io(e.to_string()))?;

        self.progress.finish(total_packages);

        Ok(lockfile)
    }

    /// Check if a package is already installed with the correct version
    fn is_installed(&self, pkg: &ResolvedPackage) -> bool {
        let pkg_dir = self.node_modules.join(&pkg.name);
        let pkg_json = pkg_dir.join("package.json");

        if !pkg_json.exists() {
            return false;
        }

        if let Ok(content) = fs::read_to_string(&pkg_json) {
            if let Ok(existing) = serde_json::from_str::<PackageJson>(&content) {
                return existing.version.as_deref() == Some(&pkg.version);
            }
        }

        false
    }

    /// Download packages in parallel
    async fn download_packages_parallel(
        &mut self,
        packages: Vec<ResolvedPackage>,
        registry: &NpmRegistry,
    ) -> Result<Vec<(ResolvedPackage, Vec<u8>)>, InstallError> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
        let mut tasks = FuturesUnordered::new();

        for pkg in packages {
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| InstallError::Io(e.to_string()))?;

            let registry = registry.clone();
            let cache_dir = self.cache_dir.clone();

            tasks.push(tokio::spawn(async move {
                let result = download_package(&pkg, &registry, &cache_dir).await;
                drop(permit);
                result.map(|tarball| (pkg, tarball))
            }));
        }

        let mut results = Vec::new();
        while let Some(result) = tasks.next().await {
            let (pkg, tarball) = result
                .map_err(|e| InstallError::Io(e.to_string()))?
                .map_err(|e| InstallError::Network(e.to_string()))?;
            results.push((pkg, tarball));
        }

        Ok(results)
    }

    /// Get cache path for a package (used in tests)
    #[allow(dead_code)]
    fn get_cache_path(&self, name: &str, version: &str) -> PathBuf {
        let safe_name = name.replace('/', "-").replace('@', "");
        self.cache_dir
            .join(format!("{}-{}.tgz", safe_name, version))
    }
}

impl Default for Installer {
    fn default() -> Self {
        Self::new(Path::new("."))
    }
}

/// Download a single package (from cache or network)
async fn download_package(
    pkg: &ResolvedPackage,
    registry: &NpmRegistry,
    cache_dir: &Path,
) -> Result<Vec<u8>, String> {
    let safe_name = pkg.name.replace('/', "-").replace('@', "");
    let cache_path = cache_dir.join(format!("{}-{}.tgz", safe_name, pkg.version));

    // Check cache first
    if cache_path.exists() {
        if let Ok(data) = fs::read(&cache_path) {
            return Ok(data);
        }
    }

    // Download from registry
    let data = registry
        .download_tarball(&pkg.name, &pkg.version)
        .await
        .map_err(|e| e.to_string())?;

    // Cache it
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&cache_path, &data).ok();

    Ok(data)
}

/// Minimal package.json structure
#[derive(Debug, serde::Deserialize)]
pub struct PackageJson {
    pub name: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies", default)]
    pub dev_dependencies: Option<HashMap<String, String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Resolve error: {0}")]
    Resolve(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_installer_new() {
        let installer = Installer::new(Path::new("/tmp/test"));
        assert_eq!(
            installer.node_modules,
            PathBuf::from("/tmp/test/node_modules")
        );
    }

    #[test]
    fn test_cache_path() {
        let installer = Installer::new(Path::new("/tmp/test"));

        let path = installer.get_cache_path("lodash", "4.17.21");
        assert!(path.to_string_lossy().contains("lodash-4.17.21.tgz"));

        let scoped = installer.get_cache_path("@types/node", "18.0.0");
        assert!(scoped.to_string_lossy().contains("types-node-18.0.0.tgz"));
    }

    #[test]
    fn test_package_json_parse() {
        let json = r#"{
            "name": "test-project",
            "version": "1.0.0",
            "dependencies": {
                "lodash": "^4.17.0"
            },
            "devDependencies": {
                "typescript": "^5.0.0"
            }
        }"#;

        let pkg: PackageJson = serde_json::from_str(json).unwrap();
        assert_eq!(pkg.name, Some("test-project".to_string()));
        assert!(pkg.dependencies.is_some());
        assert!(pkg.dev_dependencies.is_some());
    }
}
