//! Package installation

use crate::lockfile::{Lockfile, LockfileEntry};
use crate::registry::NpmRegistry;
use crate::resolver::{ResolvedPackage, Resolver};
use crate::types::install_bundled_types;
use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tar::Archive;

/// Package installer
pub struct Installer {
    registry: NpmRegistry,
    node_modules: PathBuf,
    cache_dir: PathBuf,
}

impl Installer {
    pub fn new(project_dir: &Path) -> Self {
        Self {
            registry: NpmRegistry::new(),
            node_modules: project_dir.join("node_modules"),
            cache_dir: dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("otter/packages"),
        }
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

        if deps.is_empty() {
            println!("No dependencies to install.");
            return Ok(Lockfile::new());
        }

        println!("Resolving {} dependencies...", deps.len());

        // Resolve dependencies
        let mut resolver = Resolver::new(std::mem::take(&mut self.registry));
        let resolved = resolver
            .resolve(&deps)
            .await
            .map_err(|e| InstallError::Resolve(e.to_string()))?;

        self.registry = resolver.into_registry();

        println!("Installing {} packages...", resolved.len());

        // Create node_modules
        fs::create_dir_all(&self.node_modules).map_err(|e| InstallError::Io(e.to_string()))?;

        // Install each package
        let mut lockfile = Lockfile::new();

        for pkg in &resolved {
            self.install_package(pkg).await?;

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

        // Install bundled types (@types/otter, @types/node)
        install_bundled_types(&self.node_modules).map_err(|e| InstallError::Io(e.to_string()))?;

        // Write lockfile
        let lockfile_path = package_json.parent().unwrap().join("otter.lock");
        lockfile
            .save(&lockfile_path)
            .map_err(|e| InstallError::Io(e.to_string()))?;

        println!(
            "Done! Installed {} packages + bundled types.",
            resolved.len()
        );

        Ok(lockfile)
    }

    /// Install a single package
    async fn install_package(&mut self, pkg: &ResolvedPackage) -> Result<(), InstallError> {
        let pkg_dir = if pkg.name.starts_with('@') {
            // Scoped packages: node_modules/@scope/package
            self.node_modules.join(&pkg.name)
        } else {
            self.node_modules.join(&pkg.name)
        };

        // Skip if already installed with correct version
        let pkg_json = pkg_dir.join("package.json");
        if pkg_json.exists()
            && let Ok(content) = fs::read_to_string(&pkg_json)
            && let Ok(existing) = serde_json::from_str::<PackageJson>(&content)
            && existing.version.as_deref() == Some(&pkg.version)
        {
            return Ok(());
        }

        print!("  Installing {}@{}...", pkg.name, pkg.version);

        // Check cache first
        let cache_path = self.get_cache_path(&pkg.name, &pkg.version);
        let tarball = if cache_path.exists() {
            fs::read(&cache_path).map_err(|e| InstallError::Io(e.to_string()))?
        } else {
            // Download tarball
            let data = self
                .registry
                .download_tarball(&pkg.name, &pkg.version)
                .await
                .map_err(|e| InstallError::Network(e.to_string()))?;

            // Cache it
            if let Some(parent) = cache_path.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(&cache_path, &data).ok();

            data
        };

        // Extract tarball
        self.extract_tarball(&tarball, &pkg_dir)?;

        println!(" done");
        Ok(())
    }

    /// Extract tarball to directory
    fn extract_tarball(&self, tarball: &[u8], dest: &Path) -> Result<(), InstallError> {
        // npm tarballs are gzipped
        let gz = GzDecoder::new(tarball);
        let mut archive = Archive::new(gz);

        // Create destination (including parent for scoped packages)
        if dest.exists() {
            fs::remove_dir_all(dest).map_err(|e| InstallError::Io(e.to_string()))?;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| InstallError::Io(e.to_string()))?;
        }
        fs::create_dir_all(dest).map_err(|e| InstallError::Io(e.to_string()))?;

        // Extract entries (npm tarballs have "package/" prefix)
        for entry in archive
            .entries()
            .map_err(|e| InstallError::Io(e.to_string()))?
        {
            let mut entry = entry.map_err(|e| InstallError::Io(e.to_string()))?;

            let path = entry.path().map_err(|e| InstallError::Io(e.to_string()))?;

            // Strip "package/" prefix
            let path = path.strip_prefix("package").unwrap_or(&path);
            let full_path = dest.join(path);

            // Create parent directories
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).map_err(|e| InstallError::Io(e.to_string()))?;
            }

            // Extract file
            entry
                .unpack(&full_path)
                .map_err(|e| InstallError::Io(e.to_string()))?;
        }

        Ok(())
    }

    /// Get cache path for a package
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
