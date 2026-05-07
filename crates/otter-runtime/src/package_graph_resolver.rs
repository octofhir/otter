//! Read-only package-graph resolver hook for module loading.
//!
//! `ModuleLoader` still delegates full Node-compatible filesystem and
//! `node_modules` traversal to `oxc_resolver`, but package-manager-aware runs
//! can constrain bare specifiers to an installed graph DTO first. This keeps
//! the runtime read-only: it never mutates installs or fetches packages while
//! user code is executing.
//!
//! # Contents
//! - [`resolve_from_package_graph`] resolves one bare specifier from an
//!   importer directory and import kind.
//! - package `exports` helpers implement the foundation string, subpath-map,
//!   and condition-object forms for graph-backed packages.
//!
//! # Invariants
//! - Only packages present in the graph dependency edges are considered.
//! - When a package declares `exports`, non-exported subpaths are blocked
//!   instead of falling through to filesystem subpath lookup.
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
    conditions: &[String],
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
    let candidate = package_entry_candidate(target, subpath, kind, conditions)?;
    let resolved = resolve_with_extensions(&candidate)?;
    ensure_within_package(target, &resolved)?;
    Ok(Some(resolved))
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
    conditions: &[String],
) -> Result<PathBuf, String> {
    if let Some(exports) = &package.exports {
        return package_exports_candidate(package, exports, subpath, conditions);
    }
    if let Some(subpath) = subpath {
        return Ok(package.root.join(subpath));
    }
    Ok(match kind {
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
    })
}

fn package_exports_candidate(
    package: &LoaderPackageRoot,
    exports: &serde_json::Value,
    subpath: Option<&str>,
    conditions: &[String],
) -> Result<PathBuf, String> {
    let export_subpath = subpath
        .map(|subpath| format!("./{subpath}"))
        .unwrap_or_else(|| ".".to_string());
    let target = resolve_package_exports_value(package, exports, &export_subpath, conditions)?;
    let Some(relative) = target.strip_prefix("./") else {
        return Err(format!(
            "bad export target for package `{}` subpath `{}`: `{}` must start with `./`",
            package.name, export_subpath, target
        ));
    };
    Ok(package.root.join(relative))
}

fn resolve_package_exports_value(
    package: &LoaderPackageRoot,
    value: &serde_json::Value,
    export_subpath: &str,
    conditions: &[String],
) -> Result<String, String> {
    match value {
        serde_json::Value::String(target) => {
            if export_subpath == "." {
                Ok(target.clone())
            } else {
                Err(blocked_export_message(package, export_subpath))
            }
        }
        serde_json::Value::Object(map) => {
            if is_exports_subpath_map(map) {
                let Some(target) = map.get(export_subpath) else {
                    return Err(blocked_export_message(package, export_subpath));
                };
                return resolve_conditional_export(package, target, export_subpath, conditions);
            }
            if export_subpath != "." {
                return Err(blocked_export_message(package, export_subpath));
            }
            resolve_conditional_export(package, value, export_subpath, conditions)
        }
        serde_json::Value::Null => Err(blocked_export_message(package, export_subpath)),
        _ => Err(format!(
            "unsupported export target for package `{}` subpath `{}`",
            package.name, export_subpath
        )),
    }
}

fn resolve_conditional_export(
    package: &LoaderPackageRoot,
    value: &serde_json::Value,
    export_subpath: &str,
    conditions: &[String],
) -> Result<String, String> {
    match value {
        serde_json::Value::String(target) => Ok(target.clone()),
        serde_json::Value::Object(map) => {
            for condition in conditions {
                if let Some(target) = map.get(condition) {
                    return resolve_conditional_export(package, target, export_subpath, conditions);
                }
            }
            Err(format!(
                "no matching export condition for package `{}` subpath `{}`; active conditions: {}",
                package.name,
                export_subpath,
                conditions.join(", ")
            ))
        }
        serde_json::Value::Null => Err(blocked_export_message(package, export_subpath)),
        _ => Err(format!(
            "unsupported conditional export target for package `{}` subpath `{}`",
            package.name, export_subpath
        )),
    }
}

fn is_exports_subpath_map(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    map.keys().any(|key| key == "." || key.starts_with("./"))
}

fn blocked_export_message(package: &LoaderPackageRoot, export_subpath: &str) -> String {
    format!(
        "package `{}` does not export subpath `{}`",
        package.name, export_subpath
    )
}

fn ensure_within_package(package: &LoaderPackageRoot, resolved: &Path) -> Result<(), String> {
    let root = std::fs::canonicalize(&package.root).unwrap_or_else(|_| package.root.clone());
    if resolved.starts_with(&root) {
        Ok(())
    } else {
        Err(format!(
            "resolved package target `{}` escapes package `{}` root `{}`",
            resolved.display(),
            package.name,
            root.display()
        ))
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
    use serde_json::json;

    fn esm_conditions() -> Vec<String> {
        ["otter", "import", "node", "default"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn graph_with_app_and_dep(app_root: PathBuf, dep: LoaderPackageRoot) -> LoaderPackageGraph {
        let app_id = "app@workspace:.".to_string();
        let dep_id = dep.id.clone();
        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: app_id.clone(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app_root,
            main: None,
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_package(dep);
        graph.insert_dependency(app_id, "dep", dep_id);
        graph
    }

    fn dep_root_fixture(root: PathBuf) -> LoaderPackageRoot {
        LoaderPackageRoot {
            id: "dep@npm:^1.0.0".into(),
            name: "dep".into(),
            version: "1.0.0".into(),
            root,
            main: Some("main.js".into()),
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        }
    }

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

        let graph = graph_with_app_and_dep(app_root.clone(), dep_root_fixture(dep_root.clone()));

        let resolved = resolve_from_package_graph(
            &graph,
            "dep",
            &app_root,
            ImportKind::Esm,
            &esm_conditions(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(dep_root.join("main.js")).unwrap()
        );
    }

    #[test]
    fn package_graph_string_exports_resolve_package_entry() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        let dep_root_path = dir.path().join("store/dep");
        std::fs::create_dir_all(dep_root_path.join("dist")).unwrap();
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::write(dep_root_path.join("dist/index.js"), "export let x = 1;\n").unwrap();
        let mut dep = dep_root_fixture(dep_root_path.clone());
        dep.exports = Some(json!("./dist/index.js"));
        let graph = graph_with_app_and_dep(app_root.clone(), dep);

        let resolved = resolve_from_package_graph(
            &graph,
            "dep",
            &app_root,
            ImportKind::Esm,
            &esm_conditions(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(dep_root_path.join("dist/index.js")).unwrap()
        );
    }

    #[test]
    fn package_graph_map_exports_resolve_package_entry() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        let dep_root_path = dir.path().join("store/dep");
        std::fs::create_dir_all(dep_root_path.join("dist")).unwrap();
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::write(dep_root_path.join("dist/index.js"), "export let x = 1;\n").unwrap();
        let mut dep = dep_root_fixture(dep_root_path.clone());
        dep.exports = Some(json!({ ".": "./dist/index.js" }));
        let graph = graph_with_app_and_dep(app_root.clone(), dep);

        let resolved = resolve_from_package_graph(
            &graph,
            "dep",
            &app_root,
            ImportKind::Esm,
            &esm_conditions(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(dep_root_path.join("dist/index.js")).unwrap()
        );
    }

    #[test]
    fn package_graph_condition_exports_prefer_otter_condition() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        let dep_root_path = dir.path().join("store/dep");
        std::fs::create_dir_all(&dep_root_path).unwrap();
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::write(dep_root_path.join("otter.js"), "export let x = 1;\n").unwrap();
        std::fs::write(dep_root_path.join("index.js"), "export let x = 2;\n").unwrap();
        let mut dep = dep_root_fixture(dep_root_path.clone());
        dep.exports = Some(json!({
            ".": {
                "otter": "./otter.js",
                "default": "./index.js"
            }
        }));
        let graph = graph_with_app_and_dep(app_root.clone(), dep);

        let resolved = resolve_from_package_graph(
            &graph,
            "dep",
            &app_root,
            ImportKind::Esm,
            &esm_conditions(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(dep_root_path.join("otter.js")).unwrap()
        );
    }

    #[test]
    fn package_graph_exports_block_unexported_subpaths() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        let dep_root_path = dir.path().join("store/dep");
        std::fs::create_dir_all(&dep_root_path).unwrap();
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::write(dep_root_path.join("index.js"), "export let x = 1;\n").unwrap();
        std::fs::write(dep_root_path.join("private.js"), "export let x = 2;\n").unwrap();
        let mut dep = dep_root_fixture(dep_root_path);
        dep.exports = Some(json!({ ".": "./index.js" }));
        let graph = graph_with_app_and_dep(app_root.clone(), dep);

        let err = resolve_from_package_graph(
            &graph,
            "dep/private.js",
            &app_root,
            ImportKind::Esm,
            &esm_conditions(),
        )
        .expect_err("exports must block unexported package subpaths");

        assert!(err.contains("does not export subpath `./private.js`"));
    }
}
