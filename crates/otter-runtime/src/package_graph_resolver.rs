//! Read-only package-graph resolver hook for module loading.
//!
//! `ModuleLoader` still delegates full Node-compatible package.json handling to
//! `oxc_resolver`, but package-manager-aware runs can now constrain bare
//! specifiers to an installed graph DTO first. This keeps the
//! runtime read-only: it never mutates installs or fetches packages while user
//! code is executing.
//!
//! # Contents
//! - [`resolve_from_package_graph`] resolves one bare specifier from an
//!   importer directory and import kind.
//!
//! # Invariants
//! - Only packages present in the graph dependency edges are considered.
//! - The longest containing package root wins for nested packages.
//! - Missing graph entries return `Ok(None)` so the loader can fall back to its
//!   filesystem resolver.
//!
//! # See also
//! - [`crate::module_loader`] for the host loader surface.
//! - `otter-pm` for graph reconstruction before adapting into this DTO.

use std::path::{Path, PathBuf};

use crate::module_loader::{
    ImportKind, LoaderPackageGraph, LoaderPackageRoot, resolve_with_extensions,
};

/// Resolve `specifier` from `importer_dir` through a package graph.
///
/// Returns `Ok(None)` when the graph does not contain enough information for
/// this specifier. In that case the caller should continue with the normal
/// `node_modules` resolver.
pub(crate) fn resolve_from_package_graph(
    graph: &LoaderPackageGraph,
    specifier: &str,
    importer_dir: &Path,
    kind: ImportKind,
) -> Result<Option<PathBuf>, String> {
    let Some((package_name, subpath)) = split_package_specifier(specifier) else {
        return Ok(None);
    };
    let Some(importer_package) = containing_package(graph, importer_dir) else {
        return Ok(None);
    };
    let Some(dependencies) = graph.dependencies.get(&importer_package.id) else {
        return Ok(None);
    };
    let Some(target_id) = dependencies.get(package_name) else {
        return Ok(None);
    };
    let Some(target) = graph.package(target_id) else {
        return Ok(None);
    };
    let candidate = package_entry_candidate(target, subpath, kind);
    resolve_with_extensions(&candidate).map(Some)
}

fn containing_package<'a>(
    graph: &'a LoaderPackageGraph,
    importer_dir: &Path,
) -> Option<&'a LoaderPackageRoot> {
    let importer_dir = std::fs::canonicalize(importer_dir).unwrap_or_else(|_| importer_dir.into());
    graph
        .packages
        .values()
        .filter_map(|package| {
            let root =
                std::fs::canonicalize(&package.root).unwrap_or_else(|_| package.root.clone());
            importer_dir
                .starts_with(&root)
                .then_some((root.components().count(), package))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, package)| package)
}

fn package_entry_candidate(
    package: &LoaderPackageRoot,
    subpath: Option<&str>,
    kind: ImportKind,
) -> PathBuf {
    if let Some(subpath) = subpath {
        return package.root.join(subpath);
    }
    match kind {
        ImportKind::Esm => package
            .module
            .as_ref()
            .or(package.main.as_ref())
            .map_or_else(
                || package.root.join("index"),
                |entry| package.root.join(entry),
            ),
        ImportKind::Cjs => package
            .main
            .as_ref()
            .or(package.module.as_ref())
            .map_or_else(
                || package.root.join("index"),
                |entry| package.root.join(entry),
            ),
    }
}

fn split_package_specifier(specifier: &str) -> Option<(&str, Option<&str>)> {
    if specifier.starts_with('.') || specifier.starts_with('/') || specifier.contains("://") {
        return None;
    }
    if specifier.starts_with('@') {
        let mut parts = specifier.splitn(3, '/');
        let scope = parts.next()?;
        let name = parts.next()?;
        let package_len = scope.len() + 1 + name.len();
        let subpath = parts.next().map(|_| &specifier[package_len + 1..]);
        Some((&specifier[..package_len], subpath))
    } else {
        let (name, subpath) = specifier
            .split_once('/')
            .map_or((specifier, None), |(name, subpath)| (name, Some(subpath)));
        Some((name, subpath))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_scoped_package_specifier() {
        assert_eq!(
            split_package_specifier("@scope/pkg/sub/path"),
            Some(("@scope/pkg", Some("sub/path")))
        );
    }

    #[test]
    fn graph_resolution_uses_declared_dependency_edge() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        let dep_root = dir.path().join("store/dep");
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::create_dir_all(&dep_root).unwrap();
        std::fs::write(dep_root.join("main.js"), "export let x = 1;\n").unwrap();

        let app_id = "app@workspace:.".to_string();
        let dep_id = "dep@npm:^1.0.0".to_string();
        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: app_id.clone(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app_root.clone(),
            main: None,
            module: None,
        });
        graph.insert_package(LoaderPackageRoot {
            id: dep_id.clone(),
            name: "dep".into(),
            version: "1.0.0".into(),
            root: dep_root.clone(),
            main: Some("main.js".into()),
            module: None,
        });
        graph.insert_dependency(app_id, "dep", dep_id);

        let resolved = resolve_from_package_graph(&graph, "dep", &app_root, ImportKind::Esm)
            .unwrap()
            .unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(dep_root.join("main.js")).unwrap()
        );
    }
}
