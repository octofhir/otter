//! Canonical lockfile graph + format adapters for Otter's package manager.
//!
//! # Architecture
//!
//! One in-memory graph type ([`LockfileGraph`]) is shared by every
//! consumer (resolver, linker, installer, drift detection). On-disk
//! formats are pluggable leaf modules that only know how to convert
//! between their own bytes and [`LockfileGraph`] — no format adapter
//! ever imports another.
//!
//! Supported formats (see [`LockfileKind`]):
//! - `otter-lock.yaml` / `otter.lock` (ours)
//! - `pnpm-lock.yaml` (pnpm v9) — phase 2
//! - `package-lock.json` / `npm-shrinkwrap.json` — phase 2
//! - `yarn.lock` (classic v1 + berry v2+) — phase 2
//! - `bun.lock` (text; `bun.lockb` binary format is rejected) — phase 2
//!
//! # Read-detect, write-preserve
//!
//! [`detect_existing_lockfile_kind`] identifies whichever lockfile is
//! already on disk. [`write_lockfile_preserving_existing`] writes back
//! to the same kind — a pnpm user gets `pnpm-lock.yaml` updated, not
//! a surprise `otter-lock.yaml` alongside it. Otter's own format is
//! only used when the project has no lockfile yet.

pub mod otter;

// Phase-2 adapter stubs; calls return `Error::Unsupported` until the
// real parsers land.
pub mod bun;
pub mod npm;
pub mod pnpm;
pub mod yarn;

use otter_pm_manifest::PackageJson;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Canonical graph types
// ---------------------------------------------------------------------------

/// Resolved dependency graph, format-agnostic.
///
/// `importers` keys are workspace paths (relative to the project root,
/// `"."` is the root importer). Single-project installs populate only
/// `"."`; workspaces fill in one entry per package under the root.
#[derive(Debug, Clone, Default)]
pub struct LockfileGraph {
    /// Per-importer direct deps. Monorepos populate this with one
    /// entry per workspace package (relative path); a plain project
    /// uses a single `"."` importer.
    pub importers: BTreeMap<String, Vec<DirectDep>>,

    /// Every resolved package in the graph, keyed by its `dep_path`.
    /// For registry packages `dep_path` is `<name>@<version>`; peer-
    /// context resolutions append a suffix like
    /// `react@18.2.0(react-dom@18.2.0)`.
    pub packages: BTreeMap<String, LockedPackage>,

    /// Per-graph settings (pnpm v9's `settings:` header). Round-tripped
    /// through formats that carry it.
    pub settings: LockfileSettings,

    /// Top-level overrides map (`overrides` / `pnpm.overrides` /
    /// `resolutions` flattened into `selector → spec`).
    pub overrides: BTreeMap<String, String>,

    /// Names listed in `pnpm.ignoredOptionalDependencies`.
    pub ignored_optional_dependencies: BTreeSet<String>,

    /// Per-package publish timestamps from pnpm-lock.yaml's `time:`
    /// block (`name@version → iso-8601-utc`). Used by time-based
    /// resolution modes.
    pub times: BTreeMap<String, String>,

    /// Optional deps the resolver intentionally skipped on the platform
    /// that wrote the lockfile — keyed by importer path, value is
    /// `name → specifier`. Distinct from `ignored_optional_dependencies`
    /// (which is the user's static ignore list).
    pub skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>>,

    /// Resolved catalog entries (pnpm v9's `catalogs:` block). Outer key
    /// is the catalog name, inner is the package name.
    pub catalogs: BTreeMap<String, BTreeMap<String, CatalogEntry>>,
}

/// One entry in a catalog: workspace-declared specifier + resolved version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub specifier: String,
    pub version: String,
}

/// Per-graph settings mirroring pnpm v9's `settings:` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockfileSettings {
    pub auto_install_peers: bool,
    pub exclude_links_from_lockfile: bool,
    pub lockfile_include_tarball_url: bool,
}

impl Default for LockfileSettings {
    fn default() -> Self {
        Self {
            auto_install_peers: true,
            exclude_links_from_lockfile: false,
            lockfile_include_tarball_url: false,
        }
    }
}

/// One direct (root-level or workspace-importer-level) dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectDep {
    pub name: String,
    /// Key into [`LockfileGraph::packages`] (e.g. `"is-odd@3.0.1"`).
    pub dep_path: String,
    pub dep_type: DepType,
    /// The specifier the user wrote in `package.json` at write time
    /// (e.g. `"^4.17.0"`). Populated only for formats that preserve
    /// it (pnpm v9); `None` for npm / yarn / bun.
    pub specifier: Option<String>,
}

/// Which dependency block a direct dep came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepType {
    Production,
    Dev,
    Optional,
}

/// A non-registry source for a locked package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSource {
    /// `file:<dir>` — on-disk directory, hardlink-copied into the virtual store.
    Directory(PathBuf),
    /// `file:<tarball>` — on-disk `.tgz`, extracted into the virtual store.
    Tarball(PathBuf),
    /// `link:<dir>` — plain symlink, never materialized into the virtual store.
    Link(PathBuf),
    /// `git+…` / `github:user/repo` / etc.
    Git(GitSource),
    /// `https://host/pkg.tgz`.
    RemoteTarball(RemoteTarballSource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTarballSource {
    pub url: String,
    pub integrity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    pub url: String,
    pub committish: Option<String>,
    /// 40-char commit SHA pinned by the resolver via `git ls-remote`.
    pub resolved: String,
}

impl LocalSource {
    /// Filesystem-safe dep_path key used in [`LockfileGraph::packages`]
    /// for local sources. The hash input is the POSIX-form path so a
    /// checked-in lockfile resolves to the same key cross-platform.
    pub fn dep_path(&self, name: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        match self {
            LocalSource::Git(g) => {
                hasher.update(g.url.as_bytes());
                hasher.update(b"#");
                hasher.update(g.resolved.as_bytes());
            }
            LocalSource::RemoteTarball(t) => {
                hasher.update(t.url.as_bytes());
            }
            _ => hasher.update(self.path_posix().as_bytes()),
        }
        let digest = hasher.finalize();
        let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("{name}@{}+{short}", self.kind_str())
    }

    /// Short kind tag used in `dep_path` (`file`, `link`, `git`, `url`).
    pub fn kind_str(&self) -> &'static str {
        match self {
            LocalSource::Directory(_) | LocalSource::Tarball(_) => "file",
            LocalSource::Link(_) => "link",
            LocalSource::Git(_) => "git",
            LocalSource::RemoteTarball(_) => "url",
        }
    }

    /// Cross-platform POSIX path string (forward slashes always).
    pub fn path_posix(&self) -> String {
        match self {
            LocalSource::Directory(p) | LocalSource::Tarball(p) | LocalSource::Link(p) => {
                p.to_string_lossy().replace('\\', "/")
            }
            _ => String::new(),
        }
    }
}

/// One resolved package.
#[derive(Debug, Clone, Default)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub integrity: Option<String>,
    pub dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
    pub peer_dependencies: BTreeMap<String, String>,
    pub peer_dependencies_meta: BTreeMap<String, PeerDepMeta>,
    /// Key into [`LockfileGraph::packages`] — matches the outer map's key.
    pub dep_path: String,
    /// Set for non-registry packages (`file:` / `link:` / `git:` / remote tarball).
    pub local_source: Option<LocalSource>,
    pub os: Vec<String>,
    pub cpu: Vec<String>,
    pub libc: Vec<String>,
    /// Names declared in this package's `bundledDependencies`.
    pub bundled_dependencies: Vec<String>,
    /// Full registry tarball URL (only populated when the format
    /// carries it and `LockfileSettings::lockfile_include_tarball_url`
    /// is on).
    pub tarball_url: Option<String>,
    /// When `Some`, this entry is an npm-alias (`"h3-v2": "npm:h3@2.0.1"`)
    /// and the real registry name is in here. `name` stays as the alias.
    pub alias_of: Option<String>,
    /// Yarn berry's opaque `checksum:` field — preserved verbatim for
    /// format round-trips. Not shared with `integrity` (different hash).
    pub yarn_checksum: Option<String>,
}

impl LockedPackage {
    /// Registry / CAS lookup name (honors `alias_of` when set).
    pub fn registry_name(&self) -> &str {
        self.alias_of.as_deref().unwrap_or(&self.name)
    }
}

/// `peerDependenciesMeta` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerDepMeta {
    pub optional: bool,
}

// ---------------------------------------------------------------------------
// Lockfile kind + detection
// ---------------------------------------------------------------------------

/// Which on-disk lockfile format we're looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfileKind {
    /// Otter's own format (`otter-lock.yaml` for new projects,
    /// `otter.lock` for legacy JSON format). Used only when no other
    /// lockfile is present.
    Otter,
    Pnpm,
    Npm,
    NpmShrinkwrap,
    /// `yarn.lock` classic (v1, line-based text).
    Yarn,
    /// `yarn.lock` berry (v2+, yaml).
    YarnBerry,
    /// `bun.lock` (text). Binary `bun.lockb` is rejected — see
    /// [`parse_lockfile_with_kind`].
    Bun,
}

impl LockfileKind {
    pub fn filename(self) -> &'static str {
        match self {
            LockfileKind::Otter => "otter-lock.yaml",
            LockfileKind::Pnpm => "pnpm-lock.yaml",
            LockfileKind::Npm => "package-lock.json",
            LockfileKind::NpmShrinkwrap => "npm-shrinkwrap.json",
            LockfileKind::Yarn | LockfileKind::YarnBerry => "yarn.lock",
            LockfileKind::Bun => "bun.lock",
        }
    }
}

/// Whether a lockfile matches the current manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftStatus {
    Fresh,
    Stale { reason: String },
}

// ---------------------------------------------------------------------------
// Detection + parse/write dispatchers
// ---------------------------------------------------------------------------

/// Ordered list of candidate (path, kind) pairs to check.
///
/// Precedence matches the plan:
/// 1. `otter-lock.yaml` (new format, if ever present)
/// 2. `otter.lock` (legacy JSON — transitional, Phase 1 ships this)
/// 3. `pnpm-lock.yaml`
/// 4. `bun.lock`
/// 5. `yarn.lock`
/// 6. `npm-shrinkwrap.json`
/// 7. `package-lock.json`
///
/// `include_otter` lets `otter import` skip Otter's own files so the
/// "import a foreign lockfile" workflow works even when an aube/otter
/// lockfile already exists.
fn lockfile_candidates(project_dir: &Path, include_otter: bool) -> Vec<(PathBuf, LockfileKind)> {
    let mut out = Vec::new();
    if include_otter {
        out.push((project_dir.join("otter-lock.yaml"), LockfileKind::Otter));
        out.push((project_dir.join("otter.lock"), LockfileKind::Otter));
    }
    out.push((project_dir.join("pnpm-lock.yaml"), LockfileKind::Pnpm));
    out.push((project_dir.join("bun.lock"), LockfileKind::Bun));
    out.push((project_dir.join("yarn.lock"), LockfileKind::Yarn));
    out.push((
        project_dir.join("npm-shrinkwrap.json"),
        LockfileKind::NpmShrinkwrap,
    ));
    out.push((project_dir.join("package-lock.json"), LockfileKind::Npm));
    out
}

/// Return the [`LockfileKind`] of the lockfile present in `project_dir`,
/// if any. Used by the installer to preserve a project's existing
/// format when writing back.
pub fn detect_existing_lockfile_kind(project_dir: &Path) -> Option<LockfileKind> {
    for (path, kind) in lockfile_candidates(project_dir, /*include_otter=*/ true) {
        if path.exists() {
            return Some(refine_yarn_kind(&path, kind));
        }
    }
    None
}

/// Parse the lockfile in `project_dir`, or return [`Error::NotFound`].
pub fn parse_lockfile(project_dir: &Path, manifest: &PackageJson) -> Result<LockfileGraph, Error> {
    let (graph, _kind) = parse_lockfile_with_kind(project_dir, manifest)?;
    Ok(graph)
}

/// Like [`parse_lockfile`] but also reports which format was read.
pub fn parse_lockfile_with_kind(
    project_dir: &Path,
    manifest: &PackageJson,
) -> Result<(LockfileGraph, LockfileKind), Error> {
    reject_bun_binary(project_dir)?;
    for (path, kind) in lockfile_candidates(project_dir, /*include_otter=*/ true) {
        if !path.exists() {
            continue;
        }
        let kind = refine_yarn_kind(&path, kind);
        let graph = parse_one(&path, kind, manifest)?;
        return Ok((graph, kind));
    }
    Err(Error::NotFound(project_dir.to_path_buf()))
}

/// Variant of [`parse_lockfile_with_kind`] used by `otter import` —
/// skips Otter's own files so importing a foreign lockfile into a
/// directory that *also* has `otter-lock.yaml` works.
pub fn parse_for_import(
    project_dir: &Path,
    manifest: &PackageJson,
) -> Result<(LockfileGraph, LockfileKind), Error> {
    reject_bun_binary(project_dir)?;
    for (path, kind) in lockfile_candidates(project_dir, /*include_otter=*/ false) {
        if !path.exists() {
            continue;
        }
        let kind = refine_yarn_kind(&path, kind);
        let graph = parse_one(&path, kind, manifest)?;
        return Ok((graph, kind));
    }
    Err(Error::NotFound(project_dir.to_path_buf()))
}

/// Refuse to silently fall through when only binary `bun.lockb` is
/// present — give the user an actionable error message.
fn reject_bun_binary(project_dir: &Path) -> Result<(), Error> {
    let lockb = project_dir.join("bun.lockb");
    let text = project_dir.join("bun.lock");
    if lockb.exists() && !text.exists() {
        return Err(Error::Parse(
            lockb,
            "bun.lockb (binary format) is not supported — run `bun install --save-text-lockfile` \
             to generate a bun.lock text file, or upgrade to bun 1.2+ where text is the default"
                .to_string(),
        ));
    }
    Ok(())
}

/// Peek a `yarn.lock` for the `__metadata:` marker and upgrade
/// [`LockfileKind::Yarn`] → [`LockfileKind::YarnBerry`] when present.
fn refine_yarn_kind(path: &Path, kind: LockfileKind) -> LockfileKind {
    if kind == LockfileKind::Yarn && yarn::is_berry_path(path) {
        LockfileKind::YarnBerry
    } else {
        kind
    }
}

fn parse_one(
    path: &Path,
    kind: LockfileKind,
    manifest: &PackageJson,
) -> Result<LockfileGraph, Error> {
    match kind {
        LockfileKind::Otter => otter::parse(path, manifest),
        LockfileKind::Pnpm => pnpm::parse(path),
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => npm::parse(path),
        LockfileKind::Yarn | LockfileKind::YarnBerry => yarn::parse(path, manifest),
        LockfileKind::Bun => bun::parse(path),
    }
}

/// Write the lockfile in whatever format is already on disk, falling
/// back to Otter's own format when the project has none. This is the
/// default write path for mutating commands (`install`, `add`, `remove`).
///
/// Returns the path that was written.
pub fn write_lockfile_preserving_existing(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &PackageJson,
) -> Result<PathBuf, Error> {
    let kind = detect_existing_lockfile_kind(project_dir).unwrap_or(LockfileKind::Otter);
    write_lockfile_as(project_dir, graph, manifest, kind)
}

/// Write `graph` as Otter's own format (`otter-lock.yaml` for new
/// projects, `otter.lock` for legacy JSON).
pub fn write_lockfile(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &PackageJson,
) -> Result<PathBuf, Error> {
    write_lockfile_as(project_dir, graph, manifest, LockfileKind::Otter)
}

/// Write `graph` in the requested format. Callers preserving an
/// existing lockfile should pair this with [`detect_existing_lockfile_kind`].
pub fn write_lockfile_as(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &PackageJson,
    kind: LockfileKind,
) -> Result<PathBuf, Error> {
    let filename = kind.filename();
    // For Otter-native, keep the legacy `otter.lock` filename when it
    // already exists on disk so Phase 1 doesn't force users to rename
    // their lockfile on the first `install`.
    let path = if kind == LockfileKind::Otter {
        let legacy = project_dir.join("otter.lock");
        if legacy.exists() {
            legacy
        } else {
            project_dir.join(filename)
        }
    } else {
        project_dir.join(filename)
    };
    match kind {
        LockfileKind::Otter => otter::write(&path, graph, manifest)?,
        LockfileKind::Pnpm => pnpm::write(&path, graph, manifest)?,
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => npm::write(&path, graph, manifest)?,
        LockfileKind::Yarn => yarn::write_classic(&path, graph, manifest)?,
        LockfileKind::YarnBerry => yarn::write_berry(&path, graph, manifest)?,
        LockfileKind::Bun => bun::write(&path, graph, manifest)?,
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// LockfileGraph — graph-level convenience methods
// ---------------------------------------------------------------------------

impl LockfileGraph {
    /// Direct deps of the root importer (`"."`). Empty for graphs with
    /// no root importer.
    pub fn root_deps(&self) -> &[DirectDep] {
        self.importers.get(".").map(Vec::as_slice).unwrap_or(&[])
    }

    /// Lookup a package by its dep_path key.
    pub fn get_package(&self, dep_path: &str) -> Option<&LockedPackage> {
        self.packages.get(dep_path)
    }

    /// Canonical serialization hash (SHA-256) of a JSON-normalized
    /// dump of the graph. Used for signed-lockfile integrity and for
    /// cheap equality checks between two graphs.
    pub fn checksum(&self) -> String {
        use sha2::{Digest, Sha256};
        let canonical = otter::to_canonical_json(self);
        let digest = Sha256::digest(canonical.as_bytes());
        format!("{digest:x}")
    }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no lockfile found in {0}")]
    NotFound(PathBuf),
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, String),
    #[error("failed to parse {0}: {1}")]
    Parse(PathBuf, String),
    #[error("lockfile format not yet supported: {0:?}")]
    Unsupported(LockfileKind),
}

/// Stable serde-friendly projection of [`LockfileGraph`] used by the
/// `otter` adapter (canonical JSON) and by the graph-level checksum.
/// Exposed at crate level so format adapters and integration tests can
/// round-trip through it without pulling in the adapter module.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphWire<'a> {
    pub version: u32,
    pub importers: BTreeMap<String, Vec<DirectDepWire<'a>>>,
    pub packages: BTreeMap<String, LockedPackageWire<'a>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub ignored_optional_dependencies: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub times: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DirectDepWire<'a> {
    pub name: &'a str,
    pub dep_path: &'a str,
    pub dep_type: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specifier: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LockedPackageWire<'a> {
    pub name: &'a str,
    pub version: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<&'a str>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<&'a str, &'a str>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<&'a str, &'a str>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_dependencies: BTreeMap<&'a str, &'a str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tarball_url: Option<&'a str>,
}

impl DepType {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            DepType::Production => "prod",
            DepType::Dev => "dev",
            DepType::Optional => "optional",
        }
    }

    pub(crate) fn from_wire(s: &str) -> Self {
        match s {
            "dev" => DepType::Dev,
            "optional" => DepType::Optional,
            _ => DepType::Production,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_pm_manifest::PackageJson;

    fn mk_manifest() -> PackageJson {
        let mut pkg = PackageJson {
            name: Some("root".to_string()),
            version: Some("0.0.0".to_string()),
            ..PackageJson::default()
        };
        pkg.dependencies
            .insert("lodash".to_string(), "^4".to_string());
        pkg
    }

    fn mk_graph() -> LockfileGraph {
        let mut g = LockfileGraph::default();
        g.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "lodash".to_string(),
                dep_path: "lodash@4.17.21".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^4".to_string()),
            }],
        );
        g.packages.insert(
            "lodash@4.17.21".to_string(),
            LockedPackage {
                name: "lodash".to_string(),
                version: "4.17.21".to_string(),
                integrity: Some("sha512-fake".to_string()),
                dep_path: "lodash@4.17.21".to_string(),
                ..LockedPackage::default()
            },
        );
        g
    }

    #[test]
    fn detect_no_lockfile_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_existing_lockfile_kind(dir.path()).is_none());
    }

    #[test]
    fn detect_legacy_otter_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("otter.lock"), "{}").unwrap();
        assert_eq!(
            detect_existing_lockfile_kind(dir.path()),
            Some(LockfileKind::Otter)
        );
    }

    #[test]
    fn detect_pnpm_beats_npm_and_yarn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "").unwrap();
        assert_eq!(
            detect_existing_lockfile_kind(dir.path()),
            Some(LockfileKind::Pnpm)
        );
    }

    #[test]
    fn shrinkwrap_beats_package_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("npm-shrinkwrap.json"), "{}").unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(
            detect_existing_lockfile_kind(dir.path()),
            Some(LockfileKind::NpmShrinkwrap)
        );
    }

    #[test]
    fn otter_lockfile_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let graph = mk_graph();
        let manifest = mk_manifest();
        let written =
            write_lockfile_as(dir.path(), &graph, &manifest, LockfileKind::Otter).unwrap();
        assert!(written.exists());
        let (back, kind) = parse_lockfile_with_kind(dir.path(), &manifest).unwrap();
        assert_eq!(kind, LockfileKind::Otter);
        assert_eq!(back.root_deps().len(), 1);
        assert_eq!(back.packages.len(), 1);
        assert_eq!(back.checksum(), graph.checksum());
    }

    #[test]
    fn write_preserving_creates_otter_lockfile_when_none_exists() {
        let dir = tempfile::tempdir().unwrap();
        let graph = mk_graph();
        let manifest = mk_manifest();
        let written = write_lockfile_preserving_existing(dir.path(), &graph, &manifest).unwrap();
        assert_eq!(
            detect_existing_lockfile_kind(dir.path()),
            Some(LockfileKind::Otter)
        );
        // Default new-project filename is `otter-lock.yaml`.
        assert!(
            written
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".yaml")
        );
    }

    #[test]
    fn write_preserving_uses_legacy_otter_lock_when_present() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-create the legacy file so the writer picks it.
        std::fs::write(
            dir.path().join("otter.lock"),
            "{\"version\":1,\"packages\":{}}",
        )
        .unwrap();
        let graph = mk_graph();
        let manifest = mk_manifest();
        let written = write_lockfile_preserving_existing(dir.path(), &graph, &manifest).unwrap();
        assert!(written.ends_with("otter.lock"));
    }

    #[test]
    fn bun_binary_surfaces_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bun.lockb"), b"\0\0").unwrap();
        let manifest = mk_manifest();
        let err = parse_lockfile(dir.path(), &manifest).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bun.lockb"), "got: {msg}");
    }

    #[test]
    fn parse_lockfile_not_found_is_ok_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = mk_manifest();
        match parse_lockfile(dir.path(), &manifest) {
            Err(Error::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
