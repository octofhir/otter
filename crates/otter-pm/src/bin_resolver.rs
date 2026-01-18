//! Binary resolver for finding and executing package binaries.
//!
//! Supports resolution from:
//! - Local node_modules/.bin
//! - Global exec cache (~/.cache/otter/exec-cache)

use crate::install::PackageJson;
use std::path::{Path, PathBuf};

/// Resolved binary information
#[derive(Debug, Clone)]
pub struct ResolvedBin {
    /// Binary command name
    pub name: String,
    /// Full path to the binary
    pub path: PathBuf,
    /// Package name this binary belongs to
    pub package_name: String,
    /// Package version
    pub package_version: String,
}

/// Binary resolver for finding package executables
pub struct BinResolver {
    /// Local node_modules paths to search
    local_paths: Vec<PathBuf>,
    /// Global cache directory
    cache_dir: PathBuf,
}

impl BinResolver {
    /// Create a new BinResolver starting from the given project directory
    pub fn new(project_dir: &Path) -> Self {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("otter/exec-cache");

        // Collect all node_modules paths walking up the tree
        let mut local_paths = Vec::new();
        let mut current = Some(project_dir);
        while let Some(dir) = current {
            let node_modules = dir.join("node_modules");
            if node_modules.exists() {
                local_paths.push(node_modules);
            }
            current = dir.parent();
        }

        Self {
            local_paths,
            cache_dir,
        }
    }

    /// Find binary in local node_modules/.bin
    pub fn find_local(&self, name: &str) -> Option<ResolvedBin> {
        for node_modules in &self.local_paths {
            let bin_dir = node_modules.join(".bin");

            // Check for script (Unix: no extension, Windows: .cmd)
            let script_path = bin_dir.join(name);
            if script_path.exists() {
                if let Some(resolved) = self.resolve_bin_from_shim(&script_path, name) {
                    return Some(resolved);
                }
            }

            // Check for .cmd on Windows
            #[cfg(windows)]
            {
                let cmd_path = bin_dir.join(format!("{}.cmd", name));
                if cmd_path.exists() {
                    if let Some(resolved) = self.resolve_bin_from_shim(&cmd_path, name) {
                        return Some(resolved);
                    }
                }
            }
        }
        None
    }

    /// Find binary in global exec cache
    pub fn find_cached(&self, package: &str, version: &str, cmd: &str) -> Option<ResolvedBin> {
        let cache_path = self
            .cache_dir
            .join(Self::safe_name(package))
            .join(version)
            .join("node_modules")
            .join(package);

        if !cache_path.exists() {
            return None;
        }

        self.resolve_bin_from_package(&cache_path, cmd)
    }

    /// Get the path where a package would be cached
    pub fn cache_path(&self, package: &str, version: &str) -> PathBuf {
        self.cache_dir
            .join(Self::safe_name(package))
            .join(version)
    }

    /// Check if a package is cached
    pub fn is_cached(&self, package: &str, version: &str) -> bool {
        let marker = self.cache_path(package, version).join(".installed");
        marker.exists()
    }

    /// Get the global cache directory
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Resolve a binary from a shim file in .bin directory
    fn resolve_bin_from_shim(&self, shim_path: &Path, name: &str) -> Option<ResolvedBin> {
        // Try to read the shim and find the actual package
        let content = std::fs::read_to_string(shim_path).ok()?;

        // Parse shim to find the target package
        // Typical shim: #!/usr/bin/env node
        //               "use strict";
        //               require('../typescript/lib/tsc.js');
        // Or: #!/bin/sh
        //     basedir=$(dirname "$(echo "$0" | sed -e 's,\\,/,g')")
        //     exec node  "$basedir/../typescript/lib/tsc.js" "$@"

        // Extract package name from path patterns like "../typescript/" or "typescript/"
        let package_name = if let Some(idx) = content.find("../") {
            let rest = &content[idx + 3..];
            rest.split('/').next().unwrap_or(name)
        } else {
            name
        };

        // Find the actual package directory
        for node_modules in &self.local_paths {
            let pkg_dir = node_modules.join(package_name);
            if pkg_dir.exists() {
                let pkg_json_path = pkg_dir.join("package.json");
                if let Ok(pkg_content) = std::fs::read_to_string(&pkg_json_path) {
                    if let Ok(pkg) = serde_json::from_str::<PackageJson>(&pkg_content) {
                        return Some(ResolvedBin {
                            name: name.to_string(),
                            path: shim_path.to_path_buf(),
                            package_name: pkg.name.unwrap_or_else(|| package_name.to_string()),
                            package_version: pkg.version.unwrap_or_else(|| "0.0.0".to_string()),
                        });
                    }
                }
            }
        }

        // Fallback: just return the shim path with minimal info
        Some(ResolvedBin {
            name: name.to_string(),
            path: shim_path.to_path_buf(),
            package_name: package_name.to_string(),
            package_version: "unknown".to_string(),
        })
    }

    /// Get the bin entry point from a package directory
    fn resolve_bin_from_package(&self, pkg_dir: &Path, cmd: &str) -> Option<ResolvedBin> {
        let pkg_json_path = pkg_dir.join("package.json");
        let content = std::fs::read_to_string(&pkg_json_path).ok()?;
        let pkg: PackageJson = serde_json::from_str(&content).ok()?;

        let name = pkg.name.as_ref()?;
        let version = pkg.version.as_ref()?;

        let bins = pkg.bin.as_ref()?.to_map(name);
        let bin_path = bins.get(cmd)?;

        Some(ResolvedBin {
            name: cmd.to_string(),
            path: pkg_dir.join(bin_path),
            package_name: name.clone(),
            package_version: version.clone(),
        })
    }

    /// Convert package name to safe filesystem name
    fn safe_name(name: &str) -> String {
        name.replace('/', "-").replace('@', "")
    }
}

/// List all binaries available in node_modules/.bin
pub fn list_local_binaries(project_dir: &Path) -> Vec<String> {
    let bin_dir = project_dir.join("node_modules/.bin");
    if !bin_dir.exists() {
        return Vec::new();
    }

    let mut binaries = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&bin_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip .cmd files on Unix, include them on Windows
            #[cfg(not(windows))]
            if name.ends_with(".cmd") || name.ends_with(".ps1") {
                continue;
            }
            binaries.push(name);
        }
    }
    binaries.sort();
    binaries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_safe_name() {
        assert_eq!(BinResolver::safe_name("lodash"), "lodash");
        assert_eq!(BinResolver::safe_name("@types/node"), "types-node");
        assert_eq!(BinResolver::safe_name("@scope/pkg"), "scope-pkg");
    }

    #[test]
    fn test_bin_resolver_new() {
        let dir = TempDir::new().unwrap();
        let resolver = BinResolver::new(dir.path());

        assert!(resolver.cache_dir.to_string_lossy().contains("otter"));
    }

    #[test]
    fn test_cache_path() {
        let dir = TempDir::new().unwrap();
        let resolver = BinResolver::new(dir.path());

        let path = resolver.cache_path("cowsay", "1.5.0");
        assert!(path.to_string_lossy().contains("cowsay"));
        assert!(path.to_string_lossy().contains("1.5.0"));
    }

    #[test]
    fn test_list_local_binaries() {
        let dir = TempDir::new().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("tsc"), "#!/bin/sh\necho tsc").unwrap();
        fs::write(bin_dir.join("eslint"), "#!/bin/sh\necho eslint").unwrap();

        let binaries = list_local_binaries(dir.path());
        assert!(binaries.contains(&"tsc".to_string()));
        assert!(binaries.contains(&"eslint".to_string()));
    }
}
