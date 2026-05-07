//! ES-module loader for the new engine — relative paths plus
//! npm / `node_modules` / workspace resolution.
//!
//! Relative specifiers (`./x`, `../x`) and absolute `file://`
//! URLs go through a hand-rolled path resolver with a fixed
//! foundation extension list. Bare specifiers (`import x from
//! "lodash"`), `@scope/pkg` packages, conditional `exports`
//! maps, `node_modules` walk-up, and workspace cross-references
//! go through [`oxc_resolver`] which mirrors enhanced-resolve /
//! Node.js's resolution algorithm.
//!
//! # Contents
//! - [`ModuleLoader`] — resolves + reads a specifier's source.
//! - [`ResolvedSource`] — `(url, source_kind, text)` triple.
//! - [`LoaderError`] — distinct enum for resolve / load failures.
//!
//! # Invariants
//! - All canonical URLs use the `file://` scheme with a fully
//!   canonicalised filesystem path so identity comparison is
//!   string equality. Two specifiers that point at the same
//!   underlying file always produce the same URL.
//! - Source caching is deferred to the higher-level graph driver
//!   (the loader itself is stateless).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-hostloadimportedmodule>
//!   — the host-defined module-loading hook this struct backs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use otter_syntax::SourceKind;
use oxc_resolver::{ResolveOptions, Resolver};

use crate::package_graph_resolver;

/// Which resolver flavour to use when consulting
/// [`oxc_resolver`] for a bare specifier. ESM is the default;
/// the CJS variant is wired for symmetry with `package.json`'s
/// conditional `exports` map (where `"require"` and `"import"`
/// can pick different files).
///
/// Spec mapping: matches the `Conditions` set passed to
/// `PackageExportsResolve` in
/// <https://nodejs.org/api/esm.html#resolution-and-loading-algorithm>.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// `import x from "pkg"` — apply ESM condition names.
    Esm,
    /// `require("pkg")` — apply CJS condition names. Foundation
    /// does not yet execute CJS modules; the resolver kind is
    /// present so future interop slices have a hook.
    Cjs,
}

/// Return the directory of `referrer`'s file:// URL, or `None`
/// when no referrer is set / the URL is malformed.
fn referrer_dir(referrer: Option<&str>) -> Option<PathBuf> {
    referrer
        .and_then(|r| r.strip_prefix("file://"))
        .map(Path::new)
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
}

/// One resolved + loaded module.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    /// Canonical `file://` URL of the resolved source.
    pub url: String,
    /// Source-language flavour, picked from the file's extension.
    pub kind: SourceKind,
    /// Source text (UTF-8).
    pub text: String,
}

/// Runtime-local read-only package graph used by [`ModuleLoader`].
///
/// This is a deliberately small DTO, not the package-manager model. Product
/// crates and the CLI adapt their richer install graph into this shape so
/// `otter-runtime` does not depend on registry/cache/install code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoaderPackageGraph {
    /// Packages keyed by stable package id.
    pub packages: BTreeMap<String, LoaderPackageRoot>,
    /// Dependency edges keyed by source package id, then dependency name.
    pub dependencies: BTreeMap<String, BTreeMap<String, String>>,
}

impl LoaderPackageGraph {
    /// Construct an empty package graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a package root.
    pub fn insert_package(&mut self, package: LoaderPackageRoot) {
        self.packages.insert(package.id.clone(), package);
    }

    /// Insert a dependency edge.
    pub fn insert_dependency(
        &mut self,
        from: impl Into<String>,
        name: impl Into<String>,
        target: impl Into<String>,
    ) {
        self.dependencies
            .entry(from.into())
            .or_default()
            .insert(name.into(), target.into());
    }

    /// Resolve one package by id.
    #[must_use]
    pub fn package(&self, id: &str) -> Option<&LoaderPackageRoot> {
        self.packages.get(id)
    }
}

/// One package root in [`LoaderPackageGraph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderPackageRoot {
    /// Stable package id.
    pub id: String,
    /// Package name.
    pub name: String,
    /// Package version.
    pub version: String,
    /// Materialized package root.
    pub root: PathBuf,
    /// `package.json#main`, when known.
    pub main: Option<String>,
    /// `package.json#module`, when known.
    pub module: Option<String>,
}

/// Resolve / load failure modes. The runtime maps these onto the
/// public `OtterError` shape; `LoaderError` is the loader-side
/// strongly-typed surface kept distinct so embedders that wire
/// their own loader impl have something concrete to return.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoaderError {
    /// Specifier shape is not supported by this loader (e.g. bare
    /// `lodash` against the foundation file-only loader).
    #[error("unsupported specifier shape: {specifier}")]
    UnsupportedSpecifier {
        /// The raw specifier text.
        specifier: String,
    },
    /// Resolution succeeded structurally but the resolved file
    /// could not be read (missing, permission denied, …).
    #[error("cannot load `{url}`: {message}")]
    Load {
        /// Resolved URL.
        url: String,
        /// Underlying reason.
        message: String,
    },
    /// Resolution failed: candidate file does not exist after
    /// extension resolution / index-file lookup.
    #[error("cannot resolve `{specifier}` from `{referrer}`: {message}")]
    Resolve {
        /// Raw specifier.
        specifier: String,
        /// Importer's URL (or working directory for entry).
        referrer: String,
        /// Underlying reason.
        message: String,
    },
    /// File extension is not a foundation source extension.
    #[error("unsupported source extension for `{url}`: {extension}")]
    Extension {
        /// Resolved URL.
        url: String,
        /// Offending extension.
        extension: String,
    },
}

/// Configuration for [`ModuleLoader`]. Mirrors the subset of
/// `oxc_resolver`'s [`ResolveOptions`] the foundation needs.
///
/// Locked at `Otter::new()` time — changing it mid-run would
/// produce stale entries in any future source cache (see
/// task 36b's "cache invalidation" risk).
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// Filesystem directory the entry-level loader uses as the
    /// initial referrer when no other context is available
    /// (typically the entry module's parent dir).
    pub base_dir: PathBuf,
    /// Extension list `oxc_resolver` tries when the specifier has
    /// none (e.g. `import x from "./y"` → tries `y.ts`, `y.js`,
    /// …). Order matters: TS extensions come first so a
    /// TypeScript project doesn't accidentally pull in a
    /// generated `.js` sibling.
    pub extensions: Vec<String>,
    /// Condition names the ESM resolver matches against
    /// `package.json#exports`. Default:
    /// `["import", "module", "node", "default"]`.
    pub esm_conditions: Vec<String>,
    /// Condition names the CJS resolver matches against
    /// `package.json#exports`. Default:
    /// `["require", "node", "default"]`.
    pub cjs_conditions: Vec<String>,
    /// `true` when the loader is allowed to walk `node_modules`
    /// for bare specifiers. The default, but embedders that want
    /// to ban implicit dependencies (sandbox / per-test
    /// isolation) flip this off and bare specifiers fail.
    pub enable_node_modules: bool,
    /// Runtime-provided hosted module specifiers such as `otter:kv`.
    pub hosted_specifiers: Vec<String>,
    /// Optional read-only installed package graph.
    pub package_graph: Option<LoaderPackageGraph>,
}

impl LoaderConfig {
    /// Default config rooted at `base_dir`. Matches a fresh JS
    /// project: full extension list, conventional ESM / CJS
    /// condition names, `node_modules` walk-up enabled.
    #[must_use]
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            extensions: FOUNDATION_EXTENSIONS
                .iter()
                .map(|e| format!(".{e}"))
                .collect(),
            esm_conditions: vec![
                "import".into(),
                "module".into(),
                "node".into(),
                "default".into(),
            ],
            cjs_conditions: vec!["require".into(), "node".into(), "default".into()],
            enable_node_modules: true,
            hosted_specifiers: Vec::new(),
            package_graph: None,
        }
    }
}

/// ES-module loader with relative-path + `oxc_resolver`-backed
/// bare-specifier resolution.
///
/// # Algorithm (resolve)
/// 1. If the specifier starts with `file://`, canonicalise the
///    path and return as-is.
/// 2. If it starts with `./` or `../`, resolve against the
///    referrer's parent directory through the foundation path
///    resolver (`resolve_with_extensions`).
/// 3. If it is an absolute filesystem path, canonicalise.
/// 4. If it starts with `npm:`, strip the prefix and treat as
///    a bare specifier (common runtime sugar).
/// 5. Otherwise (bare name, `@scope/name`, …): hand off to
///    [`oxc_resolver`]. The resolver walks `node_modules`
///    upward from the importer's directory, respects
///    `package.json#exports` (with the configured ESM / CJS
///    condition names), and follows workspace links from
///    `package.json#workspaces` and `pnpm-workspace.yaml`.
///
/// Spec: <https://tc39.es/ecma262/#sec-hostresolveimportedmodule>
///        <https://nodejs.org/api/modules.html#all-together>
pub struct ModuleLoader {
    config: LoaderConfig,
    esm_resolver: Resolver,
    cjs_resolver: Resolver,
}

impl std::fmt::Debug for ModuleLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleLoader")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

const FOUNDATION_EXTENSIONS: &[&str] =
    &["ts", "mts", "cts", "tsx", "js", "mjs", "cjs", "jsx", "json"];

impl ModuleLoader {
    /// Construct a loader rooted at `base_dir` with foundation
    /// defaults. Equivalent to `with_config(LoaderConfig::new(base_dir))`.
    #[must_use]
    pub fn new(base_dir: PathBuf) -> Self {
        Self::with_config(LoaderConfig::new(base_dir))
    }

    /// Construct a loader with explicit configuration.
    #[must_use]
    pub fn with_config(config: LoaderConfig) -> Self {
        let esm_options = ResolveOptions {
            extensions: config.extensions.clone(),
            condition_names: config.esm_conditions.clone(),
            ..ResolveOptions::default()
        };
        let cjs_options = ResolveOptions {
            extensions: config.extensions.clone(),
            condition_names: config.cjs_conditions.clone(),
            ..ResolveOptions::default()
        };
        Self {
            config,
            esm_resolver: Resolver::new(esm_options),
            cjs_resolver: Resolver::new(cjs_options),
        }
    }

    /// Borrow the active config.
    #[must_use]
    pub fn config(&self) -> &LoaderConfig {
        &self.config
    }

    /// `true` when `url` names a runtime-hosted module.
    #[must_use]
    pub fn is_hosted_url(&self, url: &str) -> bool {
        self.config
            .hosted_specifiers
            .iter()
            .any(|specifier| specifier == url)
    }

    /// Resolve `specifier` against `referrer` and return the
    /// canonical `file://` URL.
    ///
    /// # Errors
    /// See [`LoaderError`].
    pub fn resolve(&self, specifier: &str, referrer: Option<&str>) -> Result<String, LoaderError> {
        self.resolve_with_kind(specifier, referrer, ImportKind::Esm)
    }

    /// Resolve `specifier` honouring an explicit import kind.
    /// Used by re-exports / dynamic imports that want the ESM
    /// resolver explicitly even when the importer's file is a
    /// `.cjs`. Foundation always uses ESM today; the second
    /// resolver is wired so future CJS interop slices have one
    /// in place.
    ///
    /// # Errors
    /// See [`LoaderError`].
    pub fn resolve_with_kind(
        &self,
        specifier: &str,
        referrer: Option<&str>,
        kind: ImportKind,
    ) -> Result<String, LoaderError> {
        if self.is_hosted_url(specifier) {
            return Ok(specifier.to_string());
        }
        if specifier.starts_with("otter:") {
            return Err(LoaderError::UnsupportedSpecifier {
                specifier: specifier.to_string(),
            });
        }
        if let Some(rest) = specifier.strip_prefix("file://") {
            let path = canonicalise(Path::new(rest)).map_err(|e| LoaderError::Resolve {
                specifier: specifier.to_string(),
                referrer: referrer.unwrap_or("<entry>").to_string(),
                message: e,
            })?;
            return Ok(format!("file://{}", path.display()));
        }
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let referrer_path =
                referrer_dir(referrer).unwrap_or_else(|| self.config.base_dir.clone());
            let candidate = referrer_path.join(specifier);
            let resolved =
                resolve_with_extensions(&candidate).map_err(|e| LoaderError::Resolve {
                    specifier: specifier.to_string(),
                    referrer: referrer.unwrap_or("<entry>").to_string(),
                    message: e,
                })?;
            return Ok(format!("file://{}", resolved.display()));
        }
        if Path::new(specifier).is_absolute() {
            let path = canonicalise(Path::new(specifier)).map_err(|e| LoaderError::Resolve {
                specifier: specifier.to_string(),
                referrer: referrer.unwrap_or("<entry>").to_string(),
                message: e,
            })?;
            return Ok(format!("file://{}", path.display()));
        }
        // Bare-specifier path: walk node_modules upward from the
        // importer's directory using oxc_resolver. Unwrap the
        // `npm:` sugar prefix first.
        let bare = specifier.strip_prefix("npm:").unwrap_or(specifier);
        let dir = referrer_dir(referrer).unwrap_or_else(|| self.config.base_dir.clone());
        if let Some(graph) = &self.config.package_graph {
            if let Some(path) =
                package_graph_resolver::resolve_from_package_graph(graph, bare, &dir, kind)
                    .map_err(|message| LoaderError::Resolve {
                        specifier: specifier.to_string(),
                        referrer: referrer.unwrap_or("<entry>").to_string(),
                        message,
                    })?
            {
                return Ok(format!("file://{}", path.display()));
            }
        }
        if !self.config.enable_node_modules {
            return Err(LoaderError::UnsupportedSpecifier {
                specifier: specifier.to_string(),
            });
        }
        let resolver = match kind {
            ImportKind::Esm => &self.esm_resolver,
            ImportKind::Cjs => &self.cjs_resolver,
        };
        match resolver.resolve(&dir, bare) {
            Ok(resolution) => {
                let path =
                    canonicalise(&resolution.full_path()).map_err(|e| LoaderError::Resolve {
                        specifier: specifier.to_string(),
                        referrer: referrer.unwrap_or("<entry>").to_string(),
                        message: e,
                    })?;
                Ok(format!("file://{}", path.display()))
            }
            Err(e) => Err(LoaderError::Resolve {
                specifier: specifier.to_string(),
                referrer: referrer.unwrap_or("<entry>").to_string(),
                message: e.to_string(),
            }),
        }
    }

    /// Resolve, then read the source.
    ///
    /// # Errors
    /// See [`LoaderError`].
    pub fn load(
        &self,
        specifier: &str,
        referrer: Option<&str>,
    ) -> Result<ResolvedSource, LoaderError> {
        let url = self.resolve(specifier, referrer)?;
        if self.is_hosted_url(&url) {
            return Ok(ResolvedSource {
                url,
                kind: SourceKind::JavaScript,
                text: String::new(),
            });
        }
        let path = url.strip_prefix("file://").unwrap_or(&url);
        let extension = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        // §16.2 JSON modules — `.json` files load as a single
        // `export default <parsed>` module. The foundation realises
        // this by wrapping the raw JSON text in a module shim;
        // every JSON value (object, array, string, number, boolean,
        // null) is a valid parenthesised JS expression. Behaviour
        // matches the import-attributes proposal's `with { type:
        // "json" }` form even when the attribute is omitted, so
        // ergonomic `import data from "./x.json"` works without
        // the host having to surface the attribute parse.
        // <https://tc39.es/proposal-json-modules/>
        if extension == "json" {
            let raw = std::fs::read_to_string(path).map_err(|e| LoaderError::Load {
                url: url.clone(),
                message: e.to_string(),
            })?;
            let wrapped = format!("export default ({raw});\n");
            return Ok(ResolvedSource {
                url,
                kind: SourceKind::JavaScript,
                text: wrapped,
            });
        }
        let kind = otter_syntax::detect_source_kind(Path::new(path)).ok_or_else(|| {
            LoaderError::Extension {
                url: url.clone(),
                extension: extension.to_string(),
            }
        })?;
        let text = std::fs::read_to_string(path).map_err(|e| LoaderError::Load {
            url: url.clone(),
            message: e.to_string(),
        })?;
        Ok(ResolvedSource { url, kind, text })
    }
}

/// Canonicalise `path`, returning an absolute filesystem path.
/// Wraps `std::fs::canonicalize` with a friendlier error string.
fn canonicalise(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|e| format!("canonicalise `{}`: {e}", path.display()))
}

/// Resolve a candidate path with the foundation extension /
/// index-file lookup rules. Mirrors §HostResolveImportedModule's
/// host-policy hook for filesystem-based loaders.
pub(crate) fn resolve_with_extensions(candidate: &Path) -> Result<PathBuf, String> {
    if candidate.is_file() {
        return canonicalise(candidate);
    }
    if candidate.is_dir() {
        for ext in FOUNDATION_EXTENSIONS {
            let probe = candidate.join(format!("index.{ext}"));
            if probe.is_file() {
                return canonicalise(&probe);
            }
        }
        return Err(format!(
            "directory `{}` has no index.<ext> in {:?}",
            candidate.display(),
            FOUNDATION_EXTENSIONS
        ));
    }
    if candidate.extension().is_none() {
        for ext in FOUNDATION_EXTENSIONS {
            let mut probe = candidate.to_path_buf();
            probe.set_extension(ext);
            if probe.is_file() {
                return canonicalise(&probe);
            }
        }
    }
    Err(format!("no candidate file for `{}`", candidate.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn resolves_relative_with_extension() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("entry.ts"), "// entry").unwrap();
        std::fs::write(dir.path().join("other.ts"), "// other").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).expect("entry resolves");
        let other = loader
            .resolve("./other.ts", Some(&entry))
            .expect("sibling resolves");
        assert!(other.starts_with("file://"));
        assert!(other.ends_with("other.ts"));
    }

    #[test]
    fn extensionless_relative_picks_foundation_extension() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("entry.ts"), "// entry").unwrap();
        std::fs::write(dir.path().join("util.ts"), "// util").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let util = loader.resolve("./util", Some(&entry)).unwrap();
        assert!(util.ends_with("util.ts"));
    }

    #[test]
    fn directory_resolves_to_index() {
        let dir = temp_dir();
        let sub = dir.path().join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("index.ts"), "// pkg index").unwrap();
        std::fs::write(dir.path().join("entry.ts"), "// entry").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let pkg = loader.resolve("./pkg", Some(&entry)).unwrap();
        assert!(pkg.ends_with("index.ts"));
    }

    #[test]
    fn bare_specifier_unknown_resolves_with_error_message() {
        let dir = temp_dir();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        // `oxc_resolver` returns a resolution failure for an
        // unknown bare name (no node_modules tree).
        let err = loader
            .resolve("definitely-not-installed-pkg", None)
            .expect_err("bare specifier with no install must fail");
        assert!(matches!(err, LoaderError::Resolve { .. }));
    }

    #[test]
    fn bare_specifier_disabled_when_node_modules_off() {
        let dir = temp_dir();
        let mut config = LoaderConfig::new(dir.path().to_path_buf());
        config.enable_node_modules = false;
        let loader = ModuleLoader::with_config(config);
        let err = loader
            .resolve("lodash", None)
            .expect_err("bare specifier must be rejected when node_modules is off");
        assert!(matches!(err, LoaderError::UnsupportedSpecifier { .. }));
    }

    #[test]
    fn bare_specifier_can_resolve_from_package_graph_without_node_modules_walk() {
        let dir = temp_dir();
        let app = dir.path().join("app");
        let dep = dir.path().join("store/dep");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(app.join("entry.ts"), "// entry\n").unwrap();
        std::fs::write(dep.join("main.js"), "export let answer = 1;\n").unwrap();

        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
        });
        graph.insert_package(LoaderPackageRoot {
            id: "dep@npm:^1.0.0".into(),
            name: "dep".into(),
            version: "1.0.0".into(),
            root: dep.clone(),
            main: Some("main.js".into()),
            module: None,
        });
        graph.insert_dependency("app@workspace:.", "dep", "dep@npm:^1.0.0");

        let mut config = LoaderConfig::new(app.clone());
        config.enable_node_modules = false;
        config.package_graph = Some(graph);
        let loader = ModuleLoader::with_config(config);
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let resolved = loader.resolve("dep", Some(&entry)).unwrap();

        assert!(resolved.ends_with("main.js"), "got {resolved}");
    }

    #[test]
    fn bare_specifier_resolves_through_node_modules() {
        let dir = temp_dir();
        let pkg = dir.path().join("node_modules").join("util-pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"util-pkg","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("index.js"), "export let answer = 1;\n").unwrap();
        std::fs::write(dir.path().join("entry.ts"), "// entry\n").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let resolved = loader
            .resolve("util-pkg", Some(&entry))
            .expect("util-pkg resolves through node_modules");
        assert!(resolved.ends_with("index.js"), "got {resolved}");
    }

    #[test]
    fn npm_prefix_is_sugar_for_bare_specifier() {
        let dir = temp_dir();
        let pkg = dir.path().join("node_modules").join("named-pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"named-pkg","main":"main.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("main.js"), "export let v = 0;\n").unwrap();
        std::fs::write(dir.path().join("entry.ts"), "// entry\n").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let bare = loader.resolve("named-pkg", Some(&entry)).unwrap();
        let prefixed = loader.resolve("npm:named-pkg", Some(&entry)).unwrap();
        assert_eq!(bare, prefixed);
    }

    #[test]
    fn scoped_package_resolves() {
        let dir = temp_dir();
        let pkg = dir.path().join("node_modules").join("@scope").join("ns");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@scope/ns","main":"lib.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("lib.js"), "export let x = 9;\n").unwrap();
        std::fs::write(dir.path().join("entry.ts"), "// entry\n").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let scoped = loader.resolve("@scope/ns", Some(&entry)).unwrap();
        assert!(scoped.ends_with("lib.js"), "got {scoped}");
    }

    #[test]
    fn conditional_exports_pick_per_kind() {
        let dir = temp_dir();
        let pkg = dir.path().join("node_modules").join("dual-pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{
              "name": "dual-pkg",
              "exports": {
                ".": {
                  "import": "./esm.js",
                  "require": "./cjs.js"
                }
              }
            }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("esm.js"), "export let kind = 'esm';\n").unwrap();
        std::fs::write(pkg.join("cjs.js"), "module.exports = { kind: 'cjs' };\n").unwrap();
        std::fs::write(dir.path().join("entry.ts"), "// entry\n").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let esm = loader
            .resolve_with_kind("dual-pkg", Some(&entry), ImportKind::Esm)
            .unwrap();
        let cjs = loader
            .resolve_with_kind("dual-pkg", Some(&entry), ImportKind::Cjs)
            .unwrap();
        assert!(esm.ends_with("esm.js"), "esm got {esm}");
        assert!(cjs.ends_with("cjs.js"), "cjs got {cjs}");
    }
}
