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
//! - [`resolve_imports_from_package_graph`] resolves one `#` package import
//!   specifier from the containing package scope.
//! - package `exports` helpers implement the foundation string, subpath-map,
//!   and condition-object forms for graph-backed packages.
//! - package `imports` helpers implement exact string and condition-object
//!   forms for graph-backed packages.
//!
//! # Invariants
//! - Only packages present in the graph dependency edges are considered.
//! - Package `imports` specifiers only resolve inside the longest containing
//!   package root and only for targets rooted at `./`.
//! - When a package declares `exports`, non-exported subpaths are blocked
//!   instead of falling through to filesystem subpath lookup.
//! - The longest containing package root wins for nested packages.
//! - Graph-contained importers never fall through to filesystem package lookup
//!   for bare package specifiers.
//!
//! # See also
//! - [`crate::module_loader`] for the host loader surface.
//! - `otter-pm` for graph reconstruction before adapting into this DTO.

use std::path::{Path, PathBuf};

use crate::module_loader::{
    ImportKind, LoaderPackageDependencyKind, LoaderPackageGraph, LoaderPackageRoot,
    resolve_with_configured_extensions,
};

/// Immutable index for longest-root package-scope lookup.
///
/// Built once by [`crate::module_loader::ModuleLoader`] from the read-only
/// loader DTO, then reused for `exports` and `imports` resolution.
#[derive(Debug, Clone, Default)]
pub(crate) struct PackageScopeCache {
    roots: Vec<PackageScopeRoot>,
}

#[derive(Debug, Clone)]
struct PackageScopeRoot {
    root: PathBuf,
    depth: usize,
    package_id: String,
}

impl PackageScopeCache {
    /// Build a scope cache from graph package roots.
    #[must_use]
    pub(crate) fn from_graph(graph: &LoaderPackageGraph) -> Self {
        let mut roots = graph
            .packages
            .values()
            .map(|package| {
                let root =
                    std::fs::canonicalize(&package.root).unwrap_or_else(|_| package.root.clone());
                PackageScopeRoot {
                    depth: root.components().count(),
                    root,
                    package_id: package.id.clone(),
                }
            })
            .collect::<Vec<_>>();
        roots.sort_by(|a, b| {
            b.depth
                .cmp(&a.depth)
                .then_with(|| a.package_id.cmp(&b.package_id))
        });
        Self { roots }
    }

    pub(crate) fn containing_package<'a>(
        &self,
        graph: &'a LoaderPackageGraph,
        importer_dir: &Path,
    ) -> Option<&'a LoaderPackageRoot> {
        let importer_dir =
            std::fs::canonicalize(importer_dir).unwrap_or_else(|_| importer_dir.into());
        self.roots
            .iter()
            .find(|scope| importer_dir.starts_with(&scope.root))
            .and_then(|scope| graph.package(&scope.package_id))
    }
}

/// Resolve `specifier` from `importer_dir` through a package graph.
///
/// Returns `Ok(None)` when the specifier is not a bare package specifier or
/// the importer is outside the graph. Once an importer is inside a graph
/// package, undeclared bare dependencies are hard resolver errors and must not
/// fall through to disk `node_modules` lookup.
#[cfg(test)]
pub(crate) fn resolve_from_package_graph(
    graph: &LoaderPackageGraph,
    specifier: &str,
    importer_dir: &Path,
    kind: ImportKind,
    conditions: &[String],
) -> Result<Option<PathBuf>, String> {
    let scope_cache = PackageScopeCache::from_graph(graph);
    resolve_from_package_graph_with_scope_cache(
        graph,
        &scope_cache,
        specifier,
        importer_dir,
        kind,
        conditions,
        crate::module_loader::DEFAULT_EXTENSIONS,
    )
}

/// Resolve `specifier` from `importer_dir` through a package graph using a
/// prebuilt package-scope cache.
pub(crate) fn resolve_from_package_graph_with_scope_cache<S: AsRef<str>>(
    graph: &LoaderPackageGraph,
    scope_cache: &PackageScopeCache,
    specifier: &str,
    importer_dir: &Path,
    kind: ImportKind,
    conditions: &[String],
    extensions: &[S],
) -> Result<Option<PathBuf>, String> {
    let Some((package_name, subpath)) = split_package_specifier(specifier) else {
        return Ok(None);
    };
    let Some(importer_package) = scope_cache.containing_package(graph, importer_dir) else {
        return Ok(None);
    };
    let (target, dependency_kind) = if package_name == importer_package.name {
        (importer_package, LoaderPackageDependencyKind::Runtime)
    } else {
        let Some(target_id) = graph
            .dependencies
            .get(&importer_package.id)
            .and_then(|dependencies| dependencies.get(package_name))
        else {
            return Err(format!(
                "package `{}` does not declare dependency `{package_name}`",
                importer_package.name
            ));
        };
        let dependency_kind = graph
            .dependency_kind(&importer_package.id, package_name)
            .unwrap_or(LoaderPackageDependencyKind::Runtime);
        let Some(target) = graph.package(target_id) else {
            if dependency_kind == LoaderPackageDependencyKind::Peer
                && let Some(peer_target) = installed_package_by_name(graph, package_name)
            {
                return finish_package_resolution(
                    importer_package,
                    package_name,
                    peer_target,
                    dependency_kind,
                    subpath,
                    kind,
                    conditions,
                    extensions,
                );
            }
            return Err(format!(
                "package `{}` dependency `{package_name}` points to missing graph package `{target_id}`",
                importer_package.name
            ));
        };
        if dependency_kind == LoaderPackageDependencyKind::Peer
            && !target.root.exists()
            && let Some(peer_target) = installed_package_by_name(graph, package_name)
        {
            (peer_target, dependency_kind)
        } else {
            (target, dependency_kind)
        }
    };
    finish_package_resolution(
        importer_package,
        package_name,
        target,
        dependency_kind,
        subpath,
        kind,
        conditions,
        extensions,
    )
}

fn finish_package_resolution<S: AsRef<str>>(
    importer_package: &LoaderPackageRoot,
    package_name: &str,
    target: &LoaderPackageRoot,
    dependency_kind: LoaderPackageDependencyKind,
    subpath: Option<&str>,
    kind: ImportKind,
    conditions: &[String],
    extensions: &[S],
) -> Result<Option<PathBuf>, String> {
    ensure_dependency_root_installed(importer_package, package_name, target, dependency_kind)?;
    let candidate = package_entry_candidate(target, subpath, kind, conditions)?;
    let resolved = resolve_with_configured_extensions(&candidate, extensions)?;
    ensure_within_package(target, &resolved)?;
    Ok(Some(resolved))
}

fn installed_package_by_name<'a>(
    graph: &'a LoaderPackageGraph,
    package_name: &str,
) -> Option<&'a LoaderPackageRoot> {
    graph
        .packages
        .values()
        .find(|package| package.name == package_name && package.root.exists())
}

fn ensure_dependency_root_installed(
    importer: &LoaderPackageRoot,
    dependency_name: &str,
    target: &LoaderPackageRoot,
    dependency_kind: LoaderPackageDependencyKind,
) -> Result<(), String> {
    if target.root.exists() {
        return Ok(());
    }
    if dependency_kind == LoaderPackageDependencyKind::Optional {
        return Err(format!(
            "optional dependency `{dependency_name}` for package `{}` is not installed at `{}`",
            importer.name,
            target.root.display()
        ));
    }
    if dependency_kind == LoaderPackageDependencyKind::Peer {
        return Err(format!(
            "peer dependency `{dependency_name}` for package `{}` is not installed at `{}`",
            importer.name,
            target.root.display()
        ));
    }
    Err(format!(
        "dependency `{dependency_name}` for package `{}` is not installed at `{}`",
        importer.name,
        target.root.display()
    ))
}

/// Resolve a package `imports` specifier (`#alias`) from the containing
/// package scope.
///
/// Returns `Ok(None)` when no containing graph package exists. Once an
/// importer is inside a graph package, missing or invalid import mappings are
/// hard resolver errors and must not fall back to `node_modules`.
#[cfg(test)]
pub(crate) fn resolve_imports_from_package_graph(
    graph: &LoaderPackageGraph,
    specifier: &str,
    importer_dir: &Path,
    conditions: &[String],
) -> Result<Option<PathBuf>, String> {
    let scope_cache = PackageScopeCache::from_graph(graph);
    resolve_imports_from_package_graph_with_scope_cache(
        graph,
        &scope_cache,
        specifier,
        importer_dir,
        conditions,
        crate::module_loader::DEFAULT_EXTENSIONS,
    )
}

/// Resolve a package `imports` specifier using a prebuilt package-scope cache.
pub(crate) fn resolve_imports_from_package_graph_with_scope_cache<S: AsRef<str>>(
    graph: &LoaderPackageGraph,
    scope_cache: &PackageScopeCache,
    specifier: &str,
    importer_dir: &Path,
    conditions: &[String],
    extensions: &[S],
) -> Result<Option<PathBuf>, String> {
    if !specifier.starts_with('#') {
        return Ok(None);
    }
    let Some(importer_package) = scope_cache.containing_package(graph, importer_dir) else {
        return Ok(None);
    };
    let Some(imports) = &importer_package.imports else {
        return Err(format!(
            "package `{}` has no imports map for specifier `{specifier}`",
            importer_package.name
        ));
    };
    let target = resolve_package_imports_value(importer_package, imports, specifier, conditions)?;
    let candidate = package_target_candidate(importer_package, &target, "import", specifier)?;
    let resolved = resolve_with_configured_extensions(&candidate, extensions)?;
    ensure_within_package(importer_package, &resolved)?;
    Ok(Some(resolved))
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
    package_target_candidate(package, &target, "export", &export_subpath)
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

fn resolve_package_imports_value(
    package: &LoaderPackageRoot,
    value: &serde_json::Value,
    specifier: &str,
    conditions: &[String],
) -> Result<String, String> {
    let serde_json::Value::Object(map) = value else {
        return Err(format!(
            "unsupported imports map for package `{}`; expected object",
            package.name
        ));
    };
    let Some(target) = map.get(specifier) else {
        return Err(format!(
            "package `{}` imports map has no entry for `{specifier}`",
            package.name
        ));
    };
    resolve_conditional_import(package, target, specifier, conditions)
}

fn resolve_conditional_import(
    package: &LoaderPackageRoot,
    value: &serde_json::Value,
    specifier: &str,
    conditions: &[String],
) -> Result<String, String> {
    match value {
        serde_json::Value::String(target) => Ok(target.clone()),
        serde_json::Value::Object(map) => {
            for condition in conditions {
                if let Some(target) = map.get(condition) {
                    return resolve_conditional_import(package, target, specifier, conditions);
                }
            }
            Err(format!(
                "no matching import condition for package `{}` specifier `{}`; active conditions: {}",
                package.name,
                specifier,
                conditions.join(", ")
            ))
        }
        serde_json::Value::Null => Err(format!(
            "package `{}` imports map blocks specifier `{specifier}`",
            package.name
        )),
        _ => Err(format!(
            "unsupported import target for package `{}` specifier `{specifier}`",
            package.name
        )),
    }
}

fn package_target_candidate(
    package: &LoaderPackageRoot,
    target: &str,
    target_kind: &str,
    specifier: &str,
) -> Result<PathBuf, String> {
    let Some(relative) = target.strip_prefix("./") else {
        return Err(format!(
            "bad {target_kind} target for package `{}` specifier `{specifier}`: `{target}` must start with `./`",
            package.name
        ));
    };
    Ok(package.root.join(relative))
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
    if specifier.starts_with('.')
        || specifier.starts_with('/')
        || specifier.starts_with('#')
        || specifier.contains("://")
    {
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

    #[test]
    fn package_imports_string_mapping_resolves_from_containing_package() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(app_root.join("src")).unwrap();
        std::fs::write(app_root.join("src/alias.ts"), "export let x = 1;\n").unwrap();
        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app_root.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(json!({ "#alias": "./src/alias.ts" })),
            package_type: None,
        });

        let resolved =
            resolve_imports_from_package_graph(&graph, "#alias", &app_root, &esm_conditions())
                .unwrap()
                .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(app_root.join("src/alias.ts")).unwrap()
        );
    }

    #[test]
    fn package_imports_condition_mapping_prefers_otter() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::write(app_root.join("otter.ts"), "export let x = 1;\n").unwrap();
        std::fs::write(app_root.join("index.ts"), "export let x = 2;\n").unwrap();
        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app_root.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(json!({
                "#alias": {
                    "otter": "./otter.ts",
                    "default": "./index.ts"
                }
            })),
            package_type: None,
        });

        let resolved =
            resolve_imports_from_package_graph(&graph, "#alias", &app_root, &esm_conditions())
                .unwrap()
                .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(app_root.join("otter.ts")).unwrap()
        );
    }

    #[test]
    fn package_imports_missing_alias_reports_stable_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app_root.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(json!({ "#known": "./known.ts" })),
            package_type: None,
        });

        let err =
            resolve_imports_from_package_graph(&graph, "#missing", &app_root, &esm_conditions())
                .expect_err("missing package import must report a resolver diagnostic");

        assert_eq!(err, "package `app` imports map has no entry for `#missing`");
    }

    #[test]
    fn package_imports_invalid_target_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app_root.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(json!({ "#alias": "../escape.ts" })),
            package_type: None,
        });

        let err =
            resolve_imports_from_package_graph(&graph, "#alias", &app_root, &esm_conditions())
                .expect_err("invalid package import target must be rejected");

        assert_eq!(
            err,
            "bad import target for package `app` specifier `#alias`: `../escape.ts` must start with `./`"
        );
    }

    #[test]
    fn package_scope_cache_uses_longest_containing_root() {
        let dir = tempfile::tempdir().unwrap();
        let app_root = dir.path().join("app");
        let nested_root = app_root.join("packages/nested");
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::create_dir_all(&nested_root).unwrap();
        std::fs::write(app_root.join("app.ts"), "export let x = 1;\n").unwrap();
        std::fs::write(nested_root.join("nested.ts"), "export let x = 2;\n").unwrap();
        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app_root.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(json!({ "#alias": "./app.ts" })),
            package_type: None,
        });
        graph.insert_package(LoaderPackageRoot {
            id: "nested@workspace:packages/nested".into(),
            name: "nested".into(),
            version: "0.1.0".into(),
            root: nested_root.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(json!({ "#alias": "./nested.ts" })),
            package_type: None,
        });
        let scope_cache = PackageScopeCache::from_graph(&graph);

        let resolved = resolve_imports_from_package_graph_with_scope_cache(
            &graph,
            &scope_cache,
            "#alias",
            &nested_root,
            &esm_conditions(),
            crate::module_loader::DEFAULT_EXTENSIONS,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(nested_root.join("nested.ts")).unwrap()
        );
    }
}
