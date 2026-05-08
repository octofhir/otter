//! ES-module loader for the new engine — relative paths plus
//! npm / `node_modules` / workspace resolution.
//!
//! Relative specifiers (`./x`, `../x`), bare specifiers (`import x from
//! "lodash"`), `@scope/pkg` packages, conditional `exports` maps, package
//! `imports` maps, `node_modules` walk-up, and workspace cross-references go
//! through [`oxc_resolver`]. Absolute `file://` URLs are canonicalised by the
//! host loader. Package-manager-aware runs first consult the installed package
//! graph DTO to enforce declared dependency edges and PM diagnostics.
//!
//! # Contents
//! - [`ModuleLoader`] — resolves + reads a specifier's source.
//! - [`ResolvedSource`] — loaded source plus resolver/compiler metadata.
//! - [`LoaderError`] — distinct enum for resolve / load failures.
//!
//! # Invariants
//! - All canonical URLs use the `file://` scheme with a fully
//!   canonicalised filesystem path so identity comparison is
//!   string equality. Two specifiers that point at the same
//!   underlying file always produce the same URL.
//! - Package-scope lookup for graph-backed packages is indexed at loader
//!   construction time. Filesystem package scopes are memoized by importer
//!   directory as they are observed.
//! - Source caching is deferred to the higher-level graph driver.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-hostloadimportedmodule>
//!   — the host-defined module-loading hook this struct backs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use otter_syntax::SourceKind;
use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};

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

impl ImportKind {
    fn as_label(self) -> &'static str {
        match self {
            Self::Esm => "esm",
            Self::Cjs => "cjs",
        }
    }
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

fn referrer_file(referrer: Option<&str>) -> Option<PathBuf> {
    referrer
        .and_then(|r| r.strip_prefix("file://"))
        .map(PathBuf::from)
}

/// One resolved + loaded module.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    /// Canonical `file://` URL of the resolved source.
    pub url: String,
    /// Source-language flavour, picked from the file's extension.
    pub kind: SourceKind,
    /// Nearest merged `tsconfig.json#compilerOptions.jsx`, when present.
    ///
    /// The resolver reads this through `oxc_resolver`'s tsconfig discovery,
    /// including `extends`, so compile-time callers do not need a second
    /// tsconfig loader.
    pub jsx: Option<String>,
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
    /// Dependency edge kinds keyed by source package id, then dependency name.
    pub dependency_kinds: BTreeMap<String, BTreeMap<String, LoaderPackageDependencyKind>>,
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
        self.insert_dependency_with_kind(from, name, target, LoaderPackageDependencyKind::Runtime);
    }

    /// Insert a dependency edge with its package-manager dependency kind.
    pub fn insert_dependency_with_kind(
        &mut self,
        from: impl Into<String>,
        name: impl Into<String>,
        target: impl Into<String>,
        kind: LoaderPackageDependencyKind,
    ) {
        let from = from.into();
        let name = name.into();
        self.dependencies
            .entry(from.clone())
            .or_default()
            .insert(name.clone(), target.into());
        self.dependency_kinds
            .entry(from)
            .or_default()
            .insert(name, kind);
    }

    /// Resolve one package by id.
    #[must_use]
    pub fn package(&self, id: &str) -> Option<&LoaderPackageRoot> {
        self.packages.get(id)
    }

    /// Return the dependency kind for one edge, if the product adapter
    /// supplied it.
    #[must_use]
    pub fn dependency_kind(&self, from: &str, name: &str) -> Option<LoaderPackageDependencyKind> {
        self.dependency_kinds
            .get(from)
            .and_then(|dependencies| dependencies.get(name))
            .copied()
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
    /// `package.json#exports`, when known.
    pub exports: Option<serde_json::Value>,
    /// `package.json#imports`, when known.
    pub imports: Option<serde_json::Value>,
    /// `package.json#type`, when known.
    pub package_type: Option<LoaderPackageType>,
}

/// JavaScript package module mode from `package.json#type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderPackageType {
    /// ECMAScript module package scope.
    Module,
    /// CommonJS package scope.
    CommonJs,
}

/// Runtime-local filesystem package-scope cache.
///
/// This cache stores the nearest `package.json#type` lookup result for a
/// directory. A missing `type` field is cached as `None`; that package scope
/// still stops the parent search, matching Node's package-scope boundary.
#[derive(Debug, Default)]
struct FilesystemPackageScopeCache {
    package_types: RwLock<BTreeMap<PathBuf, Option<LoaderPackageType>>>,
}

impl FilesystemPackageScopeCache {
    fn package_type_for_path(&self, path: &Path, base_dir: &Path) -> Option<LoaderPackageType> {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| base_dir.to_path_buf());
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        if let Some(package_type) = self
            .package_types
            .read()
            .expect("package scope cache read")
            .get(&dir)
            .copied()
        {
            return package_type;
        }
        let package_type = read_filesystem_package_type(&dir);
        self.package_types
            .write()
            .expect("package scope cache write")
            .insert(dir, package_type);
        package_type
    }
}

fn read_filesystem_package_type(start_dir: &Path) -> Option<LoaderPackageType> {
    let mut dir = Some(start_dir);
    while let Some(current) = dir {
        if current.file_name().and_then(|name| name.to_str()) == Some("node_modules") {
            return None;
        }
        let package_json = current.join("package.json");
        if package_json.is_file() {
            return std::fs::read_to_string(&package_json)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(
                    |value| match value.get("type").and_then(serde_json::Value::as_str) {
                        Some("module") => Some(LoaderPackageType::Module),
                        Some("commonjs") => Some(LoaderPackageType::CommonJs),
                        _ => None,
                    },
                );
        }
        dir = current.parent();
    }
    None
}

/// Package-manager dependency edge kind mirrored into the runtime DTO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderPackageDependencyKind {
    /// `dependencies`.
    Runtime,
    /// `devDependencies`.
    Development,
    /// `peerDependencies`.
    Peer,
    /// `optionalDependencies`.
    Optional,
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
    /// `["otter", "import", "node", "default"]`.
    pub esm_conditions: Vec<String>,
    /// Condition names the CJS resolver matches against
    /// `package.json#exports`. Default:
    /// `["otter", "require", "node", "default"]`.
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
            extensions: DEFAULT_EXTENSIONS
                .iter()
                .map(|extension| (*extension).to_string())
                .collect(),
            esm_conditions: vec![
                "otter".into(),
                "import".into(),
                "node".into(),
                "default".into(),
            ],
            cjs_conditions: vec![
                "otter".into(),
                "require".into(),
                "node".into(),
                "default".into(),
            ],
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
///    referrer's parent directory through [`oxc_resolver`].
/// 3. If it is an absolute filesystem path, canonicalise.
/// 4. If it starts with `npm:`, strip the prefix and treat as
///    a bare specifier (common runtime sugar).
/// 5. If it starts with `#`, resolve through the containing
///    package's `package.json#imports` map when a package graph is
///    configured, otherwise let the filesystem resolver try.
/// 6. Otherwise (bare name, `@scope/name`, …): hand off to
///    [`oxc_resolver`]. The resolver walks `node_modules`
///    upward from the importer's directory, respects
///    `package.json#exports` / `package.json#imports` (with the
///    configured ESM / CJS condition names), and follows workspace
///    links from `package.json#workspaces` and
///    `pnpm-workspace.yaml`.
///
/// Spec: <https://tc39.es/ecma262/#sec-hostresolveimportedmodule>
///        <https://nodejs.org/api/modules.html#all-together>
pub struct ModuleLoader {
    config: LoaderConfig,
    esm_resolver: Resolver,
    cjs_resolver: Resolver,
    package_scope_cache: Option<package_graph_resolver::PackageScopeCache>,
    filesystem_package_scope_cache: FilesystemPackageScopeCache,
}

impl std::fmt::Debug for ModuleLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleLoader")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

pub(crate) const DEFAULT_EXTENSIONS: &[&str] = &[
    ".ts", ".mts", ".cts", ".tsx", ".js", ".mjs", ".cjs", ".jsx", ".json",
];

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
            main_fields: vec!["module".into(), "main".into()],
            tsconfig: Some(TsconfigDiscovery::Auto),
            ..ResolveOptions::default()
        };
        let cjs_options = ResolveOptions {
            extensions: config.extensions.clone(),
            condition_names: config.cjs_conditions.clone(),
            main_fields: vec!["main".into(), "module".into()],
            tsconfig: Some(TsconfigDiscovery::Auto),
            ..ResolveOptions::default()
        };
        let package_scope_cache = config
            .package_graph
            .as_ref()
            .map(package_graph_resolver::PackageScopeCache::from_graph);
        Self {
            config,
            esm_resolver: Resolver::new(esm_options),
            cjs_resolver: Resolver::new(cjs_options),
            package_scope_cache,
            filesystem_package_scope_cache: FilesystemPackageScopeCache::default(),
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

    /// Return the nearest package type for `path`, if known.
    ///
    /// Graph-backed package roots are consulted first through the same
    /// longest-containing-root scope cache as `exports` / `imports`
    /// resolution. When no graph scope supplies a type, the loader walks parent
    /// directories for the nearest `package.json#type` and memoizes that result
    /// by importer directory. File extensions such as `.mjs` and `.cjs` are
    /// still handled by the caller as hard overrides.
    #[must_use]
    pub fn package_type_for_path(&self, path: &Path) -> Option<LoaderPackageType> {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.config.base_dir.clone());
        if let Some(graph) = self.config.package_graph.as_ref()
            && let Some(scope_cache) = self.package_scope_cache.as_ref()
            && let Some(package_type) = scope_cache
                .containing_package(graph, &dir)
                .and_then(|package| package.package_type)
        {
            return Some(package_type);
        }
        self.filesystem_package_scope_cache
            .package_type_for_path(path, &self.config.base_dir)
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
            let resolver = match kind {
                ImportKind::Esm => &self.esm_resolver,
                ImportKind::Cjs => &self.cjs_resolver,
            };
            return match resolve_with_oxc(
                resolver,
                referrer_file(referrer).as_deref(),
                &referrer_path,
                specifier,
            ) {
                Ok(resolution) => {
                    let path = canonicalise(&resolution.full_path()).map_err(|e| {
                        LoaderError::Resolve {
                            specifier: specifier.to_string(),
                            referrer: referrer.unwrap_or("<entry>").to_string(),
                            message: e,
                        }
                    })?;
                    Ok(format!("file://{}", path.display()))
                }
                Err(e) => Err(resolve_error_with_context(
                    specifier,
                    referrer,
                    kind,
                    match kind {
                        ImportKind::Esm => &self.config.esm_conditions,
                        ImportKind::Cjs => &self.config.cjs_conditions,
                    },
                    e.to_string(),
                )),
            };
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
        let conditions = match kind {
            ImportKind::Esm => &self.config.esm_conditions,
            ImportKind::Cjs => &self.config.cjs_conditions,
        };
        if bare.starts_with('#')
            && let Some(graph) = &self.config.package_graph
            && let Some(scope_cache) = &self.package_scope_cache
            && let Some(path) =
                package_graph_resolver::resolve_imports_from_package_graph_with_scope_cache(
                    graph,
                    scope_cache,
                    bare,
                    &dir,
                    conditions,
                    &self.config.extensions,
                )
                .map_err(|message| {
                    resolve_error_with_context(specifier, referrer, kind, conditions, message)
                })?
        {
            return Ok(format!("file://{}", path.display()));
        }
        if let Some(graph) = &self.config.package_graph
            && let Some(scope_cache) = &self.package_scope_cache
        {
            if let Some(path) = package_graph_resolver::resolve_from_package_graph_with_scope_cache(
                graph,
                scope_cache,
                bare,
                &dir,
                kind,
                conditions,
                &self.config.extensions,
            )
            .map_err(|message| {
                resolve_error_with_context(specifier, referrer, kind, conditions, message)
            })? {
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
        match resolve_with_oxc(resolver, referrer_file(referrer).as_deref(), &dir, bare) {
            Ok(resolution) => {
                let path =
                    canonicalise(&resolution.full_path()).map_err(|e| LoaderError::Resolve {
                        specifier: specifier.to_string(),
                        referrer: referrer.unwrap_or("<entry>").to_string(),
                        message: e,
                    })?;
                Ok(format!("file://{}", path.display()))
            }
            Err(e) => Err(resolve_error_with_context(
                specifier,
                referrer,
                kind,
                conditions,
                e.to_string(),
            )),
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
                jsx: None,
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
                jsx: None,
                text: wrapped,
            });
        }
        let jsx = self.compiler_options_jsx_for_path(Path::new(path));
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
        Ok(ResolvedSource {
            url,
            kind,
            jsx,
            text,
        })
    }

    /// Return nearest merged `tsconfig.json#compilerOptions.jsx`, if present.
    ///
    /// This uses `oxc_resolver`'s importer-aware tsconfig discovery, so
    /// `extends` and the resolver cache stay aligned with module resolution.
    #[must_use]
    pub fn compiler_options_jsx_for_path(&self, path: &Path) -> Option<String> {
        self.esm_resolver
            .find_tsconfig(path)
            .ok()
            .flatten()
            .and_then(|tsconfig| tsconfig.compiler_options.jsx.clone())
    }
}

fn resolve_error_with_context(
    specifier: &str,
    referrer: Option<&str>,
    kind: ImportKind,
    conditions: &[String],
    message: impl Into<String>,
) -> LoaderError {
    LoaderError::Resolve {
        specifier: specifier.to_string(),
        referrer: referrer.unwrap_or("<entry>").to_string(),
        message: resolver_context_message(specifier, referrer, kind, conditions, message),
    }
}

fn resolver_context_message(
    specifier: &str,
    referrer: Option<&str>,
    kind: ImportKind,
    conditions: &[String],
    message: impl Into<String>,
) -> String {
    format!(
        "{}; resolver context: importer `{}`, specifier `{specifier}`, import kind `{}`, conditions [{}]",
        message.into(),
        referrer.unwrap_or("<entry>"),
        kind.as_label(),
        conditions.join(", ")
    )
}

fn resolve_with_oxc(
    resolver: &Resolver,
    referrer_file: Option<&Path>,
    dir: &Path,
    specifier: &str,
) -> Result<oxc_resolver::Resolution, oxc_resolver::ResolveError> {
    if let Some(referrer_file) = referrer_file {
        resolver.resolve_file(referrer_file, specifier)
    } else {
        resolver.resolve(dir, specifier)
    }
}

/// Canonicalise `path`, returning an absolute filesystem path.
/// Wraps `std::fs::canonicalize` with a friendlier error string.
fn canonicalise(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|e| format!("canonicalise `{}`: {e}", path.display()))
}

/// Resolve a candidate path with a caller-supplied extension probing list.
///
/// `extensions` accepts either `.ts` or `ts` spelling; probes preserve the
/// caller's order.
pub(crate) fn resolve_with_configured_extensions<S: AsRef<str>>(
    candidate: &Path,
    extensions: &[S],
) -> Result<PathBuf, String> {
    if candidate.is_file() {
        return canonicalise(candidate);
    }
    if candidate.is_dir() {
        for extension in extensions {
            let probe = candidate.join(format!("index{}", extension_suffix(extension.as_ref())));
            if probe.is_file() {
                return canonicalise(&probe);
            }
        }
        let extension_labels = extensions.iter().map(AsRef::as_ref).collect::<Vec<_>>();
        return Err(format!(
            "directory `{}` has no index.<ext> in {:?}",
            candidate.display(),
            extension_labels
        ));
    }
    if candidate.extension().is_none() {
        for extension in extensions {
            let mut probe = candidate.to_path_buf();
            probe.set_extension(extension.as_ref().trim_start_matches('.'));
            if probe.is_file() {
                return canonicalise(&probe);
            }
        }
    }
    Err(format!("no candidate file for `{}`", candidate.display()))
}

fn extension_suffix(extension: &str) -> String {
    if extension.starts_with('.') {
        extension.to_string()
    } else {
        format!(".{extension}")
    }
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
    fn extensionless_relative_uses_configured_extension_order() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("entry.ts"), "// entry").unwrap();
        std::fs::write(dir.path().join("util.ts"), "// ts util").unwrap();
        std::fs::write(dir.path().join("util.js"), "// js util").unwrap();
        let mut config = LoaderConfig::new(dir.path().to_path_buf());
        config.extensions = vec![".js".to_string(), ".ts".to_string()];
        let loader = ModuleLoader::with_config(config);

        let entry = loader.resolve("./entry.ts", None).unwrap();
        let util = loader.resolve("./util", Some(&entry)).unwrap();

        assert!(util.ends_with("util.js"));
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
    fn package_graph_resolution_uses_configured_extension_order() {
        let dir = temp_dir();
        let app = dir.path().join("app");
        let dep = dir.path().join("store/dep");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(app.join("entry.ts"), "// entry\n").unwrap();
        std::fs::write(dep.join("main.ts"), "export let answer = 1;\n").unwrap();
        std::fs::write(dep.join("main.js"), "export let answer = 2;\n").unwrap();

        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_package(LoaderPackageRoot {
            id: "dep@npm:^1.0.0".into(),
            name: "dep".into(),
            version: "1.0.0".into(),
            root: dep,
            main: Some("main".into()),
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_dependency("app@workspace:.", "dep", "dep@npm:^1.0.0");

        let mut config = LoaderConfig::new(app);
        config.enable_node_modules = false;
        config.extensions = vec![".js".to_string(), ".ts".to_string()];
        config.package_graph = Some(graph);
        let loader = ModuleLoader::with_config(config);

        let entry = loader.resolve("./entry.ts", None).unwrap();
        let dep = loader.resolve("dep", Some(&entry)).unwrap();

        assert!(dep.ends_with("main.js"));
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
        assert_eq!(
            err,
            LoaderError::UnsupportedSpecifier {
                specifier: "lodash".to_string(),
            }
        );
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
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_package(LoaderPackageRoot {
            id: "dep@npm:^1.0.0".into(),
            name: "dep".into(),
            version: "1.0.0".into(),
            root: dep.clone(),
            main: Some("main.js".into()),
            module: None,
            exports: None,
            imports: None,
            package_type: None,
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
    fn package_graph_blocks_undeclared_bare_dependency_when_node_modules_off() {
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
            exports: None,
            imports: None,
            package_type: None,
        });
        graph.insert_package(LoaderPackageRoot {
            id: "dep@npm:^1.0.0".into(),
            name: "dep".into(),
            version: "1.0.0".into(),
            root: dep,
            main: Some("main.js".into()),
            module: None,
            exports: None,
            imports: None,
            package_type: None,
        });

        let mut config = LoaderConfig::new(app.clone());
        config.enable_node_modules = false;
        config.package_graph = Some(graph);
        let loader = ModuleLoader::with_config(config);
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let err = loader
            .resolve("dep", Some(&entry))
            .expect_err("undeclared package edge must not resolve through graph");

        match err {
            LoaderError::Resolve { message, .. } => {
                assert!(message.contains("package `app` does not declare dependency `dep`"));
                assert!(message.contains("importer `file://"));
                assert!(message.contains("specifier `dep`"));
                assert!(message.contains("conditions [otter, import, node, default]"));
            }
            other => panic!("expected graph-gated resolve error, got {other:?}"),
        }
    }

    #[test]
    fn package_import_specifier_resolves_from_loader_graph() {
        let dir = temp_dir();
        let app = dir.path().join("app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(app.join("entry.ts"), "// entry\n").unwrap();
        std::fs::write(app.join("src/alias.ts"), "export let answer = 1;\n").unwrap();

        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(serde_json::json!({ "#alias": "./src/alias.ts" })),
            package_type: None,
        });

        let mut config = LoaderConfig::new(app.clone());
        config.enable_node_modules = false;
        config.package_graph = Some(graph);
        let loader = ModuleLoader::with_config(config);
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let resolved = loader.resolve("#alias", Some(&entry)).unwrap();

        assert!(resolved.ends_with("src/alias.ts"), "got {resolved}");
    }

    #[test]
    fn missing_package_import_reports_resolver_diagnostic() {
        let dir = temp_dir();
        let app = dir.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("entry.ts"), "// entry\n").unwrap();

        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
            exports: None,
            imports: Some(serde_json::json!({ "#known": "./known.ts" })),
            package_type: None,
        });

        let mut config = LoaderConfig::new(app.clone());
        config.enable_node_modules = false;
        config.package_graph = Some(graph);
        let loader = ModuleLoader::with_config(config);
        let entry = loader.resolve("./entry.ts", None).unwrap();
        let err = loader
            .resolve("#missing", Some(&entry))
            .expect_err("missing package import should be a stable resolve error");

        match err {
            LoaderError::Resolve { message, .. } => {
                assert!(message.contains("package `app` imports map has no entry for `#missing`"));
                assert!(message.contains("importer `file://"));
                assert!(message.contains("specifier `#missing`"));
                assert!(message.contains("conditions [otter, import, node, default]"));
            }
            other => panic!("expected resolve error, got {other:?}"),
        }
    }

    #[test]
    fn package_type_lookup_uses_longest_package_scope() {
        let dir = temp_dir();
        let app = dir.path().join("app");
        let nested = app.join("packages/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let mut graph = LoaderPackageGraph::new();
        graph.insert_package(LoaderPackageRoot {
            id: "app@workspace:.".into(),
            name: "app".into(),
            version: "0.1.0".into(),
            root: app.clone(),
            main: None,
            module: None,
            exports: None,
            imports: None,
            package_type: Some(LoaderPackageType::Module),
        });
        graph.insert_package(LoaderPackageRoot {
            id: "nested@workspace:packages/nested".into(),
            name: "nested".into(),
            version: "0.1.0".into(),
            root: nested.clone(),
            main: None,
            module: None,
            exports: None,
            imports: None,
            package_type: Some(LoaderPackageType::CommonJs),
        });

        let mut config = LoaderConfig::new(app);
        config.package_graph = Some(graph);
        let loader = ModuleLoader::with_config(config);

        assert_eq!(
            loader.package_type_for_path(&nested.join("entry.js")),
            Some(LoaderPackageType::CommonJs)
        );
    }

    #[test]
    fn package_type_lookup_reads_nearest_package_json_scope() {
        let dir = temp_dir();
        let app = dir.path().join("app");
        let nested = app.join("packages/nested/src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(app.join("package.json"), r#"{"type":"module"}"#).unwrap();
        std::fs::write(
            app.join("packages/nested/package.json"),
            r#"{"type":"commonjs"}"#,
        )
        .unwrap();

        let loader = ModuleLoader::new(app.clone());

        assert_eq!(
            loader.package_type_for_path(&app.join("src/entry.js")),
            Some(LoaderPackageType::Module)
        );
        assert_eq!(
            loader.package_type_for_path(&nested.join("entry.js")),
            Some(LoaderPackageType::CommonJs)
        );
    }

    #[test]
    fn package_type_lookup_stops_at_node_modules_boundary() {
        let dir = temp_dir();
        let app = dir.path().join("app");
        let package = app.join("node_modules/pkg");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(app.join("package.json"), r#"{"type":"module"}"#).unwrap();

        let loader = ModuleLoader::new(app);

        assert_eq!(
            loader.package_type_for_path(&package.join("index.js")),
            None
        );
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
    fn disk_package_main_fields_are_resolved_by_import_kind() {
        let dir = temp_dir();
        let pkg = dir.path().join("node_modules").join("dual-main");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"dual-main","main":"cjs.js","module":"esm.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("esm.js"), "export let kind = 'esm';\n").unwrap();
        std::fs::write(pkg.join("cjs.js"), "module.exports = { kind: 'cjs' };\n").unwrap();
        std::fs::write(dir.path().join("entry.ts"), "// entry\n").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();

        let esm = loader
            .resolve_with_kind("dual-main", Some(&entry), ImportKind::Esm)
            .unwrap();
        let cjs = loader
            .resolve_with_kind("dual-main", Some(&entry), ImportKind::Cjs)
            .unwrap();

        assert!(esm.ends_with("esm.js"), "esm got {esm}");
        assert!(cjs.ends_with("cjs.js"), "cjs got {cjs}");
    }

    #[test]
    fn tsconfig_paths_are_resolved_by_oxc_resolver() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            r#"{
              "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                  "@/*": ["src/*"]
                }
              }
            }"#,
        )
        .unwrap();
        std::fs::write(src.join("util.ts"), "export let value = 1;\n").unwrap();
        std::fs::write(dir.path().join("entry.ts"), "// entry\n").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();

        let resolved = loader.resolve("@/util", Some(&entry)).unwrap();

        assert!(resolved.ends_with("src/util.ts"), "got {resolved}");
    }

    #[test]
    fn tsconfig_extends_paths_are_resolved_by_oxc_resolver() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.path().join("tsconfig.base.json"),
            r##"{
              "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                  "#shared/*": ["src/*"]
                }
              }
            }"##,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            r#"{"extends":"./tsconfig.base.json"}"#,
        )
        .unwrap();
        std::fs::write(src.join("shared.ts"), "export let value = 1;\n").unwrap();
        std::fs::write(dir.path().join("entry.ts"), "// entry\n").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());
        let entry = loader.resolve("./entry.ts", None).unwrap();

        let resolved = loader.resolve("#shared/shared", Some(&entry)).unwrap();

        assert!(resolved.ends_with("src/shared.ts"), "got {resolved}");
    }

    #[test]
    fn tsconfig_extends_jsx_option_is_available_at_load_time() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.path().join("tsconfig.base.json"),
            r#"{
              "compilerOptions": {
                "jsx": "react-jsx"
              }
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            r#"{"extends":"./tsconfig.base.json"}"#,
        )
        .unwrap();
        std::fs::write(src.join("component.tsx"), "export const x = <div />;\n").unwrap();
        let loader = ModuleLoader::new(dir.path().to_path_buf());

        let loaded = loader.load("./src/component.tsx", None).unwrap();

        assert_eq!(loaded.kind, SourceKind::TypeScriptJsx);
        assert_eq!(loaded.jsx.as_deref(), Some("react-jsx"));
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
