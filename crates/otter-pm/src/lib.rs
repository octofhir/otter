//! Active package-manager graph and installer interfaces for Otter.
//!
//! This crate defines the public PM data model used by the runtime and CLI:
//! package graph roots, package ids, local package binaries, async registry
//! metadata/cache, registry HTTP clients, tarball cache/extraction, and the
//! initial deterministic install materializer.
//!
//! # Contents
//! - [`PackageId`] — stable package graph key.
//! - [`PackageRoot`] — resolved package root metadata.
//! - [`PackageBin`] — local executable exposed by a package.
//! - [`PackageGraph`] — read-only package graph model.
//! - [`PackageResolver`], [`PackageCache`], [`PackageInstaller`] — backend
//!   traits for later install slices.
//! - [`InstallPolicy`] — the settled install-security policy.
//!
//! # Invariants
//! - Graph maps are [`std::collections::BTreeMap`] values for deterministic
//!   traversal and diagnostics.
//! - This crate has no dependency on `crates-legacy/*`, `otter-runtime`, or
//!   `otter-vm`.
//! - Capability gates are not part of first-party install command plumbing;
//!   they apply when runtime execution consumes the graph. Lifecycle scripts
//!   run only inside explicit package-manager install operations, and only for
//!   packages the project approved by name.
//!
//! # See also
//! - [`otter-pm-manifest`](../../otter-pm-manifest/src/lib.rs)
//! - [`otter-pm-lockfile`](../../otter-pm-lockfile/src/lib.rs)

mod advisory;
mod freshness;
mod install;
mod installed_graph;
mod layout;
mod lifecycle;
mod npm_range;
mod policy;
mod registry;
mod release_age;
mod resolve;
mod script_sniff;
mod store;
mod tarball;
mod warning;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use otter_pm_lockfile::{
    LifecycleMetadata, LockedPackage, Lockfile, ResolvedSource, ResolvedSourceKind, TrustState,
    install_lifecycle_scripts,
};
use otter_pm_manifest::{
    DependencySet, PACKAGE_JSON, PackageBinManifest, PackageManifest, discover_workspaces,
};
use serde::{Deserialize, Serialize};

pub use advisory::{
    AdvisoryClient, AdvisoryQuery, FixtureAdvisoryClient, HttpAdvisoryClient, MaliciousAdvisory,
};
pub use freshness::{
    DependencyState, StaleDependency, StaleReason, inspect_installed_dependencies,
};
pub use install::{ExtractedPackage, FsPackageStore, InstalledPackage};
pub use installed_graph::{prune_removed_registry_packages, resolve_installed_project};
pub use lifecycle::{LifecycleOutcome, LifecycleRun, PendingBuild, pending_builds};
pub use npm_range::NpmRange;
pub use policy::{BuildDecision, InstallPolicy};
pub use registry::{
    FileRegistryMetadataClient, FsRegistryMetadataCache, HttpRegistryMetadataClient,
    METADATA_FRESH_FOR, MetadataFetch, NpmAttestations, NpmDist, NpmPackageVersion,
    NpmProvenance, NpmRegistryMetadata, RegistryMetadataClient,
};
pub use resolve::enrich_resolution_with_registry_metadata;
pub use script_sniff::{ScriptFinding, suspicious_script_findings};
pub use store::{ContentStore, PackageIndex, StoredFile};
pub use tarball::{FileTarballClient, HttpTarballClient, TarballFetchClient, TarballSource};
pub use warning::InstallWarning;

/// Package-manager error type.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum PackageManagerError {
    /// Filesystem operation failed.
    #[error("package-manager I/O failed for `{path}`: {message}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
    /// Manifest parse/discovery failed.
    #[error(transparent)]
    Manifest(#[from] otter_pm_manifest::ManifestError),
    /// Lockfile parse/serialize failed.
    #[error(transparent)]
    Lockfile(#[from] otter_pm_lockfile::LockfileError),
    /// Registry metadata JSON parse failed.
    #[error("invalid registry metadata for `{package}`: {message}")]
    RegistryMetadata {
        /// Package name.
        package: String,
        /// Parse or validation message.
        message: String,
    },
    /// Registry/tarball HTTP operation failed.
    #[error("HTTP fetch failed for `{url}`: {message}")]
    Http {
        /// Request URL.
        url: String,
        /// Error or HTTP status message.
        message: String,
    },
    /// Version range did not match any metadata entry.
    #[error("no registry version for `{package}` satisfies `{range}`")]
    NoMatchingVersion {
        /// Package name.
        package: String,
        /// Requested range.
        range: String,
    },
    /// Tarball bytes failed integrity verification.
    #[error("integrity verification failed for `{url}`: {message}")]
    Integrity {
        /// Tarball URL or source label.
        url: String,
        /// Failure message.
        message: String,
    },
    /// Archive extraction or package materialization failed.
    #[error("package archive operation failed for `{path}`: {message}")]
    Archive {
        /// Archive, cache, or install path involved.
        path: PathBuf,
        /// Failure message.
        message: String,
    },
    /// Lifecycle script failed.
    #[error("lifecycle script `{stage}` failed for `{package}`: {message}")]
    Lifecycle {
        /// Package id or root package label.
        package: String,
        /// Lifecycle stage.
        stage: String,
        /// Failure message.
        message: String,
    },
    /// A requested package id is absent from the graph/cache.
    #[error("unknown package id `{0}`")]
    UnknownPackage(String),
    /// A requested binary is absent.
    #[error("unknown package binary `{0}`")]
    UnknownBinary(String),
    /// Backend implementation failed.
    #[error("package-manager backend `{backend}` failed: {message}")]
    Backend {
        /// Backend name.
        backend: &'static str,
        /// Failure message.
        message: String,
    },
    /// Every satisfying version is inside the minimum-release-age window.
    #[error(
        "`{package}@{version}` was published {age_hours}h ago, inside the \
         {minimum_hours}h minimum release age"
    )]
    ReleaseTooNew {
        /// Package name.
        package: String,
        /// Rejected version.
        version: String,
        /// Age of the rejected version in whole hours.
        age_hours: u64,
        /// Configured cooling window in hours.
        minimum_hours: u64,
    },
    /// A package that previously carried publish provenance no longer does.
    #[error("`{package}` lost publish provenance between `{previous_version}` and `{version}`")]
    ProvenanceDowngrade {
        /// Package name.
        package: String,
        /// Newly resolved version without provenance.
        version: String,
        /// Previously locked version that had provenance.
        previous_version: String,
    },
    /// A registry package has no published integrity digest.
    #[error("`{package}@{version}` has no registry-published integrity digest")]
    MissingIntegrity {
        /// Package name.
        package: String,
        /// Package version.
        version: String,
    },
    /// A resolved package version is a confirmed-malicious release.
    #[error("`{package}@{version}` is a known-malicious release ({advisory})")]
    MaliciousPackage {
        /// Package name.
        package: String,
        /// Package version.
        version: String,
        /// Advisory identifier.
        advisory: String,
    },
    /// The advisory database could not be consulted.
    #[error("malicious-package advisory lookup failed: {message}")]
    AdvisoryUnavailable {
        /// Failure message.
        message: String,
    },
}

/// Stable package graph key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PackageId(String);

impl PackageId {
    /// Build a package id from a stable string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Build the root workspace package id.
    #[must_use]
    pub fn root_workspace(name: &str) -> Self {
        Self(format!("{name}@workspace:."))
    }

    /// Build a workspace package id.
    #[must_use]
    pub fn workspace(name: &str, relative_root: &Path) -> Self {
        Self(format!(
            "{name}@workspace:{}",
            relative_root.to_string_lossy().replace('\\', "/")
        ))
    }

    /// Build a local file package id.
    #[must_use]
    pub fn file(name: &str, relative_root: &Path) -> Self {
        Self(format!(
            "{name}@file:{}",
            relative_root.to_string_lossy().replace('\\', "/")
        ))
    }

    /// Build a registry range package id: a dependency not resolved yet.
    #[must_use]
    pub fn registry(name: &str, range: &str) -> Self {
        Self(format!("{name}@npm:{range}"))
    }

    /// Build a resolved registry package id: the package at one version.
    #[must_use]
    pub fn registry_version(name: &str, version: &str) -> Self {
        Self(format!("{name}@npm:{version}"))
    }

    /// Build the id of an `npm:` alias resolved to one version of the
    /// package it names.
    #[must_use]
    pub fn registry_alias(alias: &str, package: &str, version: &str) -> Self {
        Self(format!("{alias}@npm:{package}@{version}"))
    }

    /// Build a tarball package id.
    #[must_use]
    pub fn tarball(name: &str, reference: &str) -> Self {
        Self(format!("{name}@tarball:{reference}"))
    }

    /// Borrow the id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A package root known to the package graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageRoot {
    /// Stable package id.
    pub id: PackageId,
    /// Package name.
    pub name: String,
    /// Package version or opaque source version.
    pub version: String,
    /// Filesystem root for this package.
    pub root: PathBuf,
    /// Parsed package manifest.
    pub manifest: PackageManifest,
}

/// A binary exposed by a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageBin {
    /// Owning package id.
    pub package: PackageId,
    /// Binary command name.
    pub name: String,
    /// Executable path.
    pub path: PathBuf,
}

/// Dependency edge kind recorded in the package graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PackageDependencyKind {
    /// `dependencies`.
    Runtime,
    /// `devDependencies`.
    Development,
    /// `peerDependencies`.
    Peer,
    /// `optionalDependencies`.
    Optional,
}

/// Read-only package graph model consumed by runtime resolution and CLI run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PackageGraph {
    /// Packages keyed by id.
    pub packages: BTreeMap<PackageId, PackageRoot>,
    /// Package dependencies keyed by source package id, then dependency name.
    pub dependencies: BTreeMap<PackageId, BTreeMap<String, PackageId>>,
    /// Dependency edge kinds keyed by source package id, then dependency name.
    pub dependency_kinds: BTreeMap<PackageId, BTreeMap<String, PackageDependencyKind>>,
    /// Local package binaries keyed by binary name.
    pub bins: BTreeMap<String, Vec<PackageBin>>,
}

impl PackageGraph {
    /// Construct an empty package graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a package root.
    pub fn insert_package(&mut self, root: PackageRoot) {
        self.packages.insert(root.id.clone(), root);
    }

    /// Insert a dependency edge.
    pub fn insert_dependency(
        &mut self,
        from: PackageId,
        name: impl Into<String>,
        target: PackageId,
    ) {
        self.insert_dependency_with_kind(from, name, target, PackageDependencyKind::Runtime);
    }

    /// Insert a dependency edge with explicit dependency kind.
    pub fn insert_dependency_with_kind(
        &mut self,
        from: PackageId,
        name: impl Into<String>,
        target: PackageId,
        kind: PackageDependencyKind,
    ) {
        let name = name.into();
        self.dependencies
            .entry(from.clone())
            .or_default()
            .insert(name.clone(), target);
        self.dependency_kinds
            .entry(from)
            .or_default()
            .insert(name, kind);
    }

    /// Return the recorded kind for one dependency edge.
    #[must_use]
    pub fn dependency_kind(&self, from: &PackageId, name: &str) -> Option<PackageDependencyKind> {
        self.dependency_kinds
            .get(from)
            .and_then(|dependencies| dependencies.get(name))
            .copied()
    }

    /// Insert a package binary.
    pub fn insert_bin(&mut self, bin: PackageBin) {
        let bins = self.bins.entry(bin.name.clone()).or_default();
        if !bins
            .iter()
            .any(|existing| existing.package == bin.package && existing.path == bin.path)
        {
            bins.push(bin);
        }
        for bins in self.bins.values_mut() {
            bins.sort_by(|a, b| a.package.cmp(&b.package).then(a.path.cmp(&b.path)));
        }
    }

    /// Resolve one package id.
    #[must_use]
    pub fn package(&self, id: &PackageId) -> Option<&PackageRoot> {
        self.packages.get(id)
    }

    /// Resolve binaries by command name.
    #[must_use]
    pub fn resolve_bin(&self, name: &str) -> &[PackageBin] {
        self.bins.get(name).map_or(&[], Vec::as_slice)
    }
}

/// Package-resolution request.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveRequest {
    /// Project root.
    pub project_root: PathBuf,
    /// Root manifest.
    pub manifest: PackageManifest,
    /// Existing lockfile, when present.
    pub lockfile: Option<Lockfile>,
}

/// Package install request.
#[derive(Debug, Clone, PartialEq)]
pub struct InstallRequest {
    /// Project root.
    pub project_root: PathBuf,
    /// Graph to materialize.
    pub graph: PackageGraph,
    /// Lockfile to write or verify.
    pub lockfile: Lockfile,
}

/// Result of an install operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// Number of packages newly materialized.
    pub added_packages: usize,
    /// Number of packages reused from cache or existing install.
    pub reused_packages: usize,
    /// Number of package binaries linked into the project-local bin directory.
    pub linked_bins: usize,
    /// Number of lifecycle scripts executed.
    pub lifecycle_scripts: usize,
    /// Packages whose install scripts were skipped pending approval.
    pub pending_builds: Vec<PendingBuild>,
    /// Non-fatal findings raised by the security gates.
    pub warnings: Vec<InstallWarning>,
    /// Whether the lockfile changed.
    pub lockfile_changed: bool,
    /// Source lockfile format imported during this install, if any.
    pub imported_lockfile: Option<otter_pm_lockfile::LockfileFormat>,
    /// Packages the previous lockfile installed that this one no longer has.
    pub pruned: PruneReport,
}

/// Result of pruning packages no longer present in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    /// Number of project-local package roots removed.
    pub removed_packages: usize,
    /// Number of project-local binary links removed.
    pub removed_bins: usize,
}

/// Result of local project graph/lockfile resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalResolution {
    /// Read-only graph for runtime/CLI consumers.
    pub graph: PackageGraph,
    /// Deterministic lockfile model.
    pub lockfile: Lockfile,
}

/// Resolve, cache metadata, download/extract registry tarballs, materialize
/// `node_modules`, and write a deterministic `otter.lock`.
///
/// Security gates run before any dependency code executes: versions inside the
/// cooling window are held back, a package that loses publish provenance fails
/// resolution, confirmed-malicious releases are refused, and install scripts
/// run only for packages the project approved.
pub async fn install_local_project(
    project_root: impl AsRef<Path>,
    metadata_cache: &FsRegistryMetadataCache,
    metadata_client: &impl RegistryMetadataClient,
    package_store: &FsPackageStore,
    tarball_client: &impl TarballFetchClient,
    advisory_client: &impl AdvisoryClient,
) -> Result<InstallReport, PackageManagerError> {
    let project_root = project_root.as_ref();
    let policy = InstallPolicy::from_manifest(&PackageManifest::read_from_dir(project_root).await?);
    let previous_lockfile = read_project_lockfile(project_root).await?;
    let mut warnings = Vec::new();
    let (mut resolution, imported_lockfile) = if previous_lockfile.is_none()
        && let Some((format, mut lockfile)) = read_migration_lockfile(project_root).await?
    {
        enrich_imported_lockfile_with_registry_metadata(
            &mut lockfile,
            metadata_cache,
            metadata_client,
        )
        .await?;
        let mut resolution = resolve_local_project(project_root).await?;
        resolution.lockfile = lockfile;
        (resolution, Some(format))
    } else {
        (
            resolve_local_project_with_registry_metadata(
                project_root,
                metadata_cache,
                metadata_client,
                &policy,
                previous_lockfile.as_ref(),
                &mut warnings,
            )
            .await?,
            None,
        )
    };
    check_provenance_continuity(previous_lockfile.as_ref(), &resolution.lockfile, &policy)?;
    check_advisories(
        &resolution.lockfile,
        previous_lockfile.as_ref(),
        advisory_client,
        &policy,
        &mut warnings,
    )
    .await?;
    check_store_integrity(&resolution.lockfile, &policy, &mut warnings)?;

    let mut final_installed = BTreeMap::new();
    let mut added_package_ids = BTreeSet::new();
    let mut completed = false;
    for _ in 0..32 {
        let installed = package_store
            .materialize_registry_packages(project_root, &resolution.lockfile, tarball_client)
            .await?;
        for package in &installed {
            if !package.reused_install {
                added_package_ids.insert(package.package_id.clone());
            }
            final_installed.insert(package.package_id.clone(), package.clone());
        }
        if !apply_tarball_manifest_metadata(project_root, &mut resolution, &installed).await? {
            completed = true;
            break;
        }
        enrich_resolution_with_registry_metadata(
            &mut resolution,
            metadata_cache,
            metadata_client,
            &policy,
            previous_lockfile.as_ref(),
            &mut warnings,
        )
        .await?;
    }
    if !completed {
        return Err(PackageManagerError::Backend {
            backend: "install",
            message: "dependency graph did not converge after 32 tarball metadata passes"
                .to_string(),
        });
    }
    layout::link_project(project_root, &resolution.graph, &resolution.lockfile).await?;
    let pruned = match &previous_lockfile {
        Some(previous) => {
            prune_removed_registry_packages(project_root, previous, &resolution.lockfile).await?
        }
        None => PruneReport {
            removed_packages: 0,
            removed_bins: 0,
        },
    };
    let lifecycle_outcome = lifecycle::run_install_lifecycle_scripts(
        project_root,
        &mut resolution.lockfile,
        &final_installed,
        &policy,
    )
    .await?;
    let lockfile_changed = write_lockfile_if_changed(project_root, &resolution.lockfile).await?;
    Ok(InstallReport {
        added_packages: added_package_ids.len(),
        reused_packages: final_installed
            .len()
            .saturating_sub(added_package_ids.len()),
        linked_bins: final_installed
            .values()
            .map(|package| package.linked_bins)
            .sum(),
        lifecycle_scripts: lifecycle_outcome.runs.len(),
        pending_builds: lifecycle_outcome.pending,
        warnings,
        lockfile_changed,
        imported_lockfile,
        pruned,
    })
}

async fn read_project_lockfile(
    project_root: &Path,
) -> Result<Option<Lockfile>, PackageManagerError> {
    let path = project_root.join(otter_pm_lockfile::LOCKFILE_NAME);
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => Lockfile::parse_toml(&text).map(Some).map_err(Into::into),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(PackageManagerError::Io {
            path,
            message: err.to_string(),
        }),
    }
}

/// Refuse a resolution in which a package that previously shipped publish
/// provenance no longer does.
///
/// Losing provenance is how a compromised publish looks from the outside: the
/// name and the range still match, but the chain back to a build no longer
/// exists. Gaining provenance, or never having had it, is not a downgrade.
fn check_provenance_continuity(
    previous: Option<&Lockfile>,
    current: &Lockfile,
    policy: &InstallPolicy,
) -> Result<(), PackageManagerError> {
    if policy.trust_policy == otter_pm_manifest::TrustPolicy::Off {
        return Ok(());
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    let mut attested_before = BTreeMap::<&str, &str>::new();
    for package in previous.packages.values() {
        if package.attested {
            attested_before.insert(package.name.as_str(), package.version.as_str());
        }
    }
    for package in current.packages.values() {
        if package.attested {
            continue;
        }
        let Some(previous_version) = attested_before.get(package.name.as_str()) else {
            continue;
        };
        return Err(PackageManagerError::ProvenanceDowngrade {
            package: package.name.clone(),
            version: package.version.clone(),
            previous_version: (*previous_version).to_string(),
        });
    }
    Ok(())
}

/// Refuse any resolved version the advisory database calls malicious.
///
/// A version the `previous` lockfile already held was asked about when it
/// entered the project; only the versions this resolution brings in are
/// asked about now, so an unchanged project installs without a lookup.
async fn check_advisories(
    lockfile: &Lockfile,
    previous: Option<&Lockfile>,
    client: &impl AdvisoryClient,
    policy: &InstallPolicy,
    warnings: &mut Vec<InstallWarning>,
) -> Result<(), PackageManagerError> {
    if policy.advisory_check == otter_pm_manifest::AdvisoryCheck::Off {
        return Ok(());
    }
    let mut queries = lockfile
        .packages
        .iter()
        .filter(|(id, package)| {
            matches!(
                package.resolved.as_ref().map(|source| source.kind),
                Some(ResolvedSourceKind::Registry)
            ) && previous
                .and_then(|previous| previous.packages.get(id.as_str()))
                .is_none_or(|held| held.version != package.version)
        })
        .map(|(_, package)| package)
        .map(|package| AdvisoryQuery {
            name: package.name.clone(),
            version: package.version.clone(),
        })
        .collect::<Vec<_>>();
    queries.sort();
    queries.dedup();
    if queries.is_empty() {
        return Ok(());
    }
    match client.query_malicious(&queries).await {
        Ok(advisories) => {
            if let Some(advisory) = advisories.first() {
                return Err(PackageManagerError::MaliciousPackage {
                    package: advisory.name.clone(),
                    version: advisory.version.clone(),
                    advisory: advisory.id.clone(),
                });
            }
            Ok(())
        }
        Err(err) if policy.advisory_check == otter_pm_manifest::AdvisoryCheck::Required => Err(err),
        Err(err) => {
            warnings.push(InstallWarning::new(
                "PM_ADVISORY_LOOKUP_FAILED",
                format!("malicious-package advisories were not consulted: {err}"),
            ));
            Ok(())
        }
    }
}

/// Report registry packages the registry published without an integrity
/// digest, so nothing verifies the bytes that arrive.
fn check_store_integrity(
    lockfile: &Lockfile,
    policy: &InstallPolicy,
    warnings: &mut Vec<InstallWarning>,
) -> Result<(), PackageManagerError> {
    for package in lockfile.packages.values() {
        let is_registry = matches!(
            package.resolved.as_ref().map(|source| source.kind),
            Some(ResolvedSourceKind::Registry)
        );
        if !is_registry || package.integrity.is_some() {
            continue;
        }
        if policy.strict_store_integrity {
            return Err(PackageManagerError::MissingIntegrity {
                package: package.name.clone(),
                version: package.version.clone(),
            });
        }
        warnings.push(InstallWarning::new(
            "PM_MISSING_INTEGRITY",
            format!(
                "`{}@{}` has no registry-published integrity digest",
                package.name, package.version
            ),
        ));
    }
    Ok(())
}

async fn read_migration_lockfile(
    project_root: &Path,
) -> Result<Option<(otter_pm_lockfile::LockfileFormat, Lockfile)>, PackageManagerError> {
    for format in [
        otter_pm_lockfile::LockfileFormat::Pnpm,
        otter_pm_lockfile::LockfileFormat::NpmShrinkwrap,
        otter_pm_lockfile::LockfileFormat::PackageLock,
    ] {
        let path = project_root.join(format.filename());
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(PackageManagerError::Io {
                    path,
                    message: err.to_string(),
                });
            }
        };
        let lockfile = Lockfile::parse_format(format, &text)?;
        return Ok(Some((format, lockfile)));
    }
    Ok(None)
}

async fn enrich_imported_lockfile_with_registry_metadata(
    lockfile: &mut Lockfile,
    metadata_cache: &FsRegistryMetadataCache,
    metadata_client: &impl RegistryMetadataClient,
) -> Result<(), PackageManagerError> {
    for package in lockfile.packages.values_mut() {
        let Some(ResolvedSource {
            kind: ResolvedSourceKind::Registry,
            reference,
        }) = &package.resolved
        else {
            continue;
        };
        let has_tarball = is_tarball_reference(reference);
        let needs_metadata = !has_tarball || package.integrity.is_none();
        if !needs_metadata {
            continue;
        }
        let metadata = metadata_cache
            .get_or_fetch(&package.name, metadata_client)
            .await?;
        let version = metadata.versions.get(&package.version).ok_or_else(|| {
            PackageManagerError::NoMatchingVersion {
                package: package.name.clone(),
                range: format!("={}", package.version),
            }
        })?;
        if !has_tarball {
            package.resolved = version.dist.tarball.as_ref().map(|tarball| ResolvedSource {
                kind: ResolvedSourceKind::Registry,
                reference: tarball.clone(),
            });
        }
        if package.integrity.is_none() {
            package.integrity = version
                .dist
                .integrity
                .clone()
                .or_else(|| version.dist.shasum.as_ref().map(|s| format!("sha1-{s}")));
        }
        package.attested = version.has_provenance();
        package.lifecycle = LifecycleMetadata::from_scripts(&version.scripts, TrustState::Unknown);
    }
    Ok(())
}

async fn apply_tarball_manifest_metadata(
    project_root: &Path,
    resolution: &mut LocalResolution,
    installed: &[InstalledPackage],
) -> Result<bool, PackageManagerError> {
    let mut changed = false;
    for package in installed {
        let Some(locked) = resolution.lockfile.packages.get(&package.package_id) else {
            continue;
        };
        let Some(source_kind) = locked
            .resolved
            .as_ref()
            .filter(|source| is_tarball_reference(&source.reference))
            .map(|source| source.kind)
        else {
            continue;
        };
        let manifest = PackageManifest::read_from_dir(&package.installed_root).await?;
        let id = PackageId::new(&package.package_id);
        // A registry package's name, version and edges come from its registry
        // manifest; a tarball named directly reveals them once unpacked.
        let direct = source_kind == ResolvedSourceKind::Tarball;
        if let Some(root) = resolution.graph.packages.get_mut(&id) {
            if direct {
                root.name = manifest.name.clone().unwrap_or_else(|| root.name.clone());
                root.version = manifest
                    .version
                    .clone()
                    .unwrap_or_else(|| root.version.clone());
            }
            root.manifest = manifest.clone();
        }
        insert_bins_for_manifest(
            &mut resolution.graph,
            &id,
            &package.installed_root,
            &manifest,
        );
        if let Some(locked) = resolution.lockfile.packages.get_mut(&package.package_id) {
            if direct {
                if let Some(name) = &manifest.name {
                    locked.name = name.clone();
                }
                if let Some(version) = &manifest.version {
                    locked.version = version.clone();
                }
            }
            locked.lifecycle =
                lifecycle_metadata_for_manifest(&manifest, Some(&package.installed_root));
        }
        if !direct {
            continue;
        }
        // The unpacked manifest is the same on every pass: only the edges not
        // yet linked are new.
        let linked = resolution.graph.dependencies.get(&id).cloned();
        let workspace_by_name: BTreeMap<String, &otter_pm_manifest::WorkspacePackage> =
            BTreeMap::new();
        for (bucket_name, dependencies) in manifest.dependency_buckets() {
            let kind = dependency_kind_for_bucket(bucket_name);
            let fresh: DependencySet = dependencies
                .iter()
                .filter(|(name, _)| {
                    linked
                        .as_ref()
                        .is_none_or(|linked| !linked.contains_key(*name))
                })
                .map(|(name, spec)| (name.clone(), spec.clone()))
                .collect();
            if kind == PackageDependencyKind::Development || fresh.is_empty() {
                continue;
            }
            changed = true;
            resolve_dependency_bucket(
                project_root,
                &mut resolution.graph,
                &mut resolution.lockfile,
                &id,
                &fresh,
                kind,
                &workspace_by_name,
            )
            .await?;
        }
    }
    Ok(changed)
}

/// Resolve the current local project into a deterministic graph and
/// lockfile. Registry dependencies are recorded as desired graph entries; no
/// network fetch or tarball extraction happens in this slice.
pub async fn resolve_local_project(
    project_root: impl AsRef<Path>,
) -> Result<LocalResolution, PackageManagerError> {
    let project_root = project_root.as_ref();
    let root_manifest = PackageManifest::read_from_dir(project_root).await?;
    let mut graph = PackageGraph::new();
    let mut lockfile = Lockfile::new();
    let root_name = root_manifest
        .name
        .clone()
        .unwrap_or_else(|| "root".to_string());
    let root_version = root_manifest
        .version
        .clone()
        .unwrap_or_else(|| "0.0.0".to_string());
    let root_id = PackageId::root_workspace(&root_name);
    let workspaces = discover_workspaces(project_root).await?;
    let mut workspace_by_name = BTreeMap::new();
    for workspace in &workspaces {
        if let Some(name) = &workspace.manifest.name {
            workspace_by_name.insert(name.clone(), workspace);
        }
    }

    graph.insert_package(PackageRoot {
        id: root_id.clone(),
        name: root_name.clone(),
        version: root_version.clone(),
        root: project_root.to_path_buf(),
        manifest: root_manifest.clone(),
    });
    insert_bins_for_manifest(&mut graph, &root_id, project_root, &root_manifest);
    lockfile.packages.insert(
        root_id.to_string(),
        locked_package(
            &root_name,
            &root_version,
            ResolvedSourceKind::Workspace,
            ".",
            &root_manifest,
        ),
    );

    for workspace in &workspaces {
        let Some(name) = workspace.manifest.name.clone() else {
            continue;
        };
        let version = workspace
            .manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_string());
        let id = PackageId::workspace(&name, &workspace.relative_root);
        graph.insert_package(PackageRoot {
            id: id.clone(),
            name: name.clone(),
            version: version.clone(),
            root: workspace.root.clone(),
            manifest: workspace.manifest.clone(),
        });
        insert_bins_for_manifest(&mut graph, &id, &workspace.root, &workspace.manifest);
        lockfile.packages.insert(
            id.to_string(),
            locked_package(
                &name,
                &version,
                ResolvedSourceKind::Workspace,
                &workspace.relative_root.to_string_lossy().replace('\\', "/"),
                &workspace.manifest,
            ),
        );
    }

    resolve_manifest_dependencies(
        project_root,
        &mut graph,
        &mut lockfile,
        &root_id,
        &root_manifest,
        &workspace_by_name,
    )
    .await?;
    for workspace in &workspaces {
        if let Some(name) = &workspace.manifest.name {
            let id = PackageId::workspace(name, &workspace.relative_root);
            resolve_manifest_dependencies(
                project_root,
                &mut graph,
                &mut lockfile,
                &id,
                &workspace.manifest,
                &workspace_by_name,
            )
            .await?;
        }
    }

    Ok(LocalResolution { graph, lockfile })
}

async fn write_lockfile_if_changed(
    project_root: &Path,
    lockfile: &Lockfile,
) -> Result<bool, PackageManagerError> {
    let text = lockfile.to_toml_string()?;
    let path = project_root.join(otter_pm_lockfile::LOCKFILE_NAME);
    let previous = match tokio::fs::read_to_string(&path).await {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(PackageManagerError::Io {
                path,
                message: err.to_string(),
            });
        }
    };
    if previous.as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    tokio::fs::write(&path, text)
        .await
        .map_err(|err| PackageManagerError::Io {
            path,
            message: err.to_string(),
        })?;
    Ok(true)
}

/// Resolve a local project, fetch/cache registry metadata, and enrich registry
/// lockfile entries with selected version, tarball, integrity, scripts, and
/// bin metadata. This does not download tarballs or execute lifecycle scripts.
/// What `pinned` resolved stands where the ranges still admit it.
pub async fn resolve_local_project_with_registry_metadata(
    project_root: impl AsRef<Path>,
    cache: &FsRegistryMetadataCache,
    client: &impl RegistryMetadataClient,
    policy: &InstallPolicy,
    pinned: Option<&Lockfile>,
    warnings: &mut Vec<InstallWarning>,
) -> Result<LocalResolution, PackageManagerError> {
    let mut resolution = resolve_local_project(project_root).await?;
    enrich_resolution_with_registry_metadata(
        &mut resolution,
        cache,
        client,
        policy,
        pinned,
        warnings,
    )
    .await?;
    Ok(resolution)
}

/// Resolver backend interface.
pub trait PackageResolver {
    /// Resolve a project manifest into a package graph and lockfile.
    fn resolve<'a>(
        &'a self,
        request: ResolveRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<(PackageGraph, Lockfile), PackageManagerError>> + Send + 'a>,
    >;
}

pub(crate) fn infer_project_root(graph: &PackageGraph) -> PathBuf {
    graph
        .packages
        .iter()
        .find_map(|(id, package)| {
            if id.as_str().ends_with("@workspace:.") {
                Some(package.root.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            graph
                .packages
                .values()
                .next()
                .map(|package| package.root.clone())
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn is_tarball_reference(reference: &str) -> bool {
    reference.ends_with(".tgz")
        || reference.ends_with(".tar.gz")
        || reference
            .strip_prefix("file:")
            .is_some_and(|path| path.ends_with(".tgz") || path.ends_with(".tar.gz"))
        || reference.starts_with("http://")
        || reference.starts_with("https://")
}

/// Cache root shared by every project belonging to this user.
///
/// The content store only pays off when checkouts share it, so it lives beside
/// the user's other caches rather than inside one project.
#[must_use]
pub fn user_cache_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("otter")
}

pub(crate) fn cache_key(package: &str) -> String {
    let mut out = String::new();
    for byte in package.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn resolve_manifest_dependencies(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    from: &PackageId,
    manifest: &PackageManifest,
    workspace_by_name: &BTreeMap<String, &otter_pm_manifest::WorkspacePackage>,
) -> Result<(), PackageManagerError> {
    for (bucket_name, dependencies) in manifest.dependency_buckets() {
        resolve_dependency_bucket(
            project_root,
            graph,
            lockfile,
            from,
            dependencies,
            dependency_kind_for_bucket(bucket_name),
            workspace_by_name,
        )
        .await?;
    }
    Ok(())
}

async fn resolve_dependency_bucket(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    from: &PackageId,
    dependencies: &DependencySet,
    dependency_kind: PackageDependencyKind,
    workspace_by_name: &BTreeMap<String, &otter_pm_manifest::WorkspacePackage>,
) -> Result<(), PackageManagerError> {
    for (name, range) in dependencies {
        let target = if range.starts_with("workspace:") {
            workspace_by_name
                .get(name)
                .map(|workspace| PackageId::workspace(name, &workspace.relative_root))
                .unwrap_or_else(|| PackageId::registry(name, range))
        } else if range
            .strip_prefix("file:")
            .is_some_and(is_tarball_reference)
        {
            let id = PackageId::tarball(name, range);
            ensure_tarball_package(project_root, graph, lockfile, &id, name, range);
            id
        } else if let Some(file_path) = range.strip_prefix("file:") {
            resolve_file_dependency(project_root, graph, lockfile, name, file_path).await?
        } else if is_tarball_reference(range) {
            let id = PackageId::tarball(name, range);
            ensure_tarball_package(project_root, graph, lockfile, &id, name, range);
            id
        } else {
            let id = PackageId::registry(name, range);
            ensure_registry_package(project_root, graph, lockfile, &id, name, range);
            id
        };
        graph.insert_dependency_with_kind(
            from.clone(),
            name.clone(),
            target.clone(),
            dependency_kind,
        );
        if let Some(package) = lockfile.packages.get_mut(from.as_str()) {
            package
                .dependencies
                .insert(name.clone(), target.to_string());
        }
    }
    Ok(())
}

fn dependency_kind_for_bucket(bucket_name: &str) -> PackageDependencyKind {
    match bucket_name {
        "devDependencies" => PackageDependencyKind::Development,
        "peerDependencies" => PackageDependencyKind::Peer,
        "optionalDependencies" => PackageDependencyKind::Optional,
        _ => PackageDependencyKind::Runtime,
    }
}

async fn resolve_file_dependency(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    dependency_name: &str,
    file_path: &str,
) -> Result<PackageId, PackageManagerError> {
    let package_root = project_root.join(file_path);
    let manifest_path = package_root.join(PACKAGE_JSON);
    if !tokio::fs::try_exists(&manifest_path)
        .await
        .map_err(|err| PackageManagerError::Io {
            path: manifest_path.clone(),
            message: err.to_string(),
        })?
    {
        let id = PackageId::file(dependency_name, Path::new(file_path));
        ensure_file_package(
            graph,
            lockfile,
            &id,
            dependency_name,
            "0.0.0",
            &package_root,
            file_path,
            &PackageManifest::default(),
        );
        return Ok(id);
    }
    let manifest = PackageManifest::read_from_dir(&package_root).await?;
    let name = manifest
        .name
        .clone()
        .unwrap_or_else(|| dependency_name.to_string());
    let version = manifest
        .version
        .clone()
        .unwrap_or_else(|| "0.0.0".to_string());
    let id = PackageId::file(&name, Path::new(file_path));
    ensure_file_package(
        graph,
        lockfile,
        &id,
        &name,
        &version,
        &package_root,
        file_path,
        &manifest,
    );
    Ok(id)
}

fn ensure_registry_package(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    id: &PackageId,
    name: &str,
    range: &str,
) {
    if graph.packages.contains_key(id) {
        return;
    }
    let manifest = PackageManifest {
        name: Some(name.to_string()),
        version: Some(range.to_string()),
        ..PackageManifest::default()
    };
    graph.insert_package(PackageRoot {
        id: id.clone(),
        name: name.to_string(),
        version: range.to_string(),
        root: project_root
            .join("node_modules")
            .join(package_name_path(name)),
        manifest: manifest.clone(),
    });
    lockfile.packages.insert(
        id.to_string(),
        locked_package(name, range, ResolvedSourceKind::Registry, range, &manifest),
    );
}

pub(crate) fn ensure_tarball_package(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    id: &PackageId,
    name: &str,
    reference: &str,
) {
    if graph.packages.contains_key(id) {
        return;
    }
    let manifest = PackageManifest {
        name: Some(name.to_string()),
        version: Some(reference.to_string()),
        ..PackageManifest::default()
    };
    graph.insert_package(PackageRoot {
        id: id.clone(),
        name: name.to_string(),
        version: reference.to_string(),
        root: layout::package_dir(project_root, id.as_str(), name),
        manifest: manifest.clone(),
    });
    lockfile.packages.insert(
        id.to_string(),
        locked_package(
            name,
            reference,
            ResolvedSourceKind::Tarball,
            reference,
            &manifest,
        ),
    );
}

fn ensure_file_package(
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    id: &PackageId,
    name: &str,
    version: &str,
    root: &Path,
    reference: &str,
    manifest: &PackageManifest,
) {
    if !graph.packages.contains_key(id) {
        graph.insert_package(PackageRoot {
            id: id.clone(),
            name: name.to_string(),
            version: version.to_string(),
            root: root.to_path_buf(),
            manifest: manifest.clone(),
        });
        insert_bins_for_manifest(graph, id, root, manifest);
    }
    lockfile.packages.entry(id.to_string()).or_insert_with(|| {
        locked_package(name, version, ResolvedSourceKind::File, reference, manifest)
    });
}

fn locked_package(
    name: &str,
    version: &str,
    kind: ResolvedSourceKind,
    reference: &str,
    manifest: &PackageManifest,
) -> LockedPackage {
    LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        dependencies: BTreeMap::new(),
        integrity: None,
        attested: false,
        resolved: Some(ResolvedSource {
            kind,
            reference: reference.to_string(),
        }),
        lifecycle: lifecycle_metadata_for_manifest(manifest, None),
    }
}

fn lifecycle_metadata_for_manifest(
    manifest: &PackageManifest,
    package_root: Option<&Path>,
) -> LifecycleMetadata {
    let mut scripts = install_lifecycle_scripts(&manifest.scripts);
    if !scripts.contains_key("preinstall")
        && !scripts.contains_key("install")
        && package_root.is_some_and(|root| root.join("binding.gyp").is_file())
    {
        scripts.insert("install".to_string(), "node-gyp rebuild".to_string());
    }
    LifecycleMetadata::from_install_scripts(scripts, TrustState::Unknown)
}

pub(crate) fn insert_bins_for_manifest(
    graph: &mut PackageGraph,
    package_id: &PackageId,
    package_root: &Path,
    manifest: &PackageManifest,
) {
    let Some(bin) = &manifest.bin else {
        return;
    };
    match bin {
        PackageBinManifest::Path(path) => {
            if let Some(name) = &manifest.name {
                graph.insert_bin(PackageBin {
                    package: package_id.clone(),
                    name: binary_name_from_package_name(name).to_string(),
                    path: package_root.join(path),
                });
            }
        }
        PackageBinManifest::Map(bins) => {
            for (name, path) in bins {
                graph.insert_bin(PackageBin {
                    package: package_id.clone(),
                    name: name.clone(),
                    path: package_root.join(path),
                });
            }
        }
    }
}

pub(crate) fn binary_name_from_package_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

pub(crate) fn package_name_path(name: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for segment in name.split('/') {
        path.push(segment);
    }
    path
}

/// Content-addressed package cache interface.
pub trait PackageCache {
    /// Return a cached package root for `id`, when present.
    fn get<'a>(
        &'a self,
        id: &'a PackageId,
    ) -> Pin<Box<dyn Future<Output = Result<Option<PackageRoot>, PackageManagerError>> + Send + 'a>>;

    /// Store a package root in the cache.
    fn put<'a>(
        &'a self,
        root: &'a PackageRoot,
    ) -> Pin<Box<dyn Future<Output = Result<(), PackageManagerError>> + Send + 'a>>;
}

/// Install pipeline backend interface.
pub trait PackageInstaller {
    /// Materialize an install request.
    fn install<'a>(
        &'a self,
        request: InstallRequest,
    ) -> Pin<Box<dyn Future<Output = Result<InstallReport, PackageManagerError>> + Send + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::Engine;
    use sha2::{Digest, Sha512};

    fn manifest(name: &str) -> PackageManifest {
        PackageManifest {
            name: Some(name.to_string()),
            version: Some("1.0.0".to_string()),
            ..PackageManifest::default()
        }
    }

    #[test]
    fn graph_resolves_packages_and_bins_deterministically() {
        let mut graph = PackageGraph::new();
        let app = PackageId::new("app@workspace:.");
        let tool = PackageId::new("tool@1.0.0");
        graph.insert_package(PackageRoot {
            id: app.clone(),
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            root: PathBuf::from("."),
            manifest: manifest("app"),
        });
        graph.insert_package(PackageRoot {
            id: tool.clone(),
            name: "tool".to_string(),
            version: "1.0.0".to_string(),
            root: PathBuf::from("node_modules/tool"),
            manifest: manifest("tool"),
        });
        graph.insert_dependency(app.clone(), "tool", tool.clone());
        graph.insert_bin(PackageBin {
            package: tool.clone(),
            name: "tool".to_string(),
            path: PathBuf::from("node_modules/.bin/tool"),
        });

        assert_eq!(graph.package(&app).unwrap().name, "app");
        assert_eq!(
            graph.dependencies[&app]["tool"],
            PackageId::new("tool@1.0.0")
        );
        assert_eq!(graph.resolve_bin("tool")[0].package, tool);
        assert!(graph.resolve_bin("missing").is_empty());
    }

    #[tokio::test]
    async fn local_project_resolution_records_workspace_file_and_registry_deps() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("package.json"),
            r#"{
              "name": "app",
              "version": "0.1.0",
              "workspaces": ["packages/*"],
              "dependencies": {
                "lib": "workspace:*",
                "file-tool": "file:tools/file-tool",
                "left-pad": "^1.3.0"
              },
              "scripts": {
                "preinstall": "node preinstall.js",
                "build": "tsc",
                "postinstall": "node postinstall.js"
              }
            }"#,
        )
        .await;
        write(
            &tmp.path().join("packages/lib/package.json"),
            r#"{"name":"lib","version":"1.0.0","bin":{"lib":"./bin.js"}}"#,
        )
        .await;
        write(
            &tmp.path().join("tools/file-tool/package.json"),
            r#"{"name":"file-tool","version":"2.0.0","bin":"./cli.js"}"#,
        )
        .await;

        let resolved = resolve_local_project(tmp.path()).await.unwrap();
        let app = PackageId::root_workspace("app");
        assert_eq!(
            resolved.graph.dependencies[&app]["lib"],
            PackageId::workspace("lib", Path::new("packages/lib"))
        );
        assert_eq!(
            resolved.graph.dependencies[&app]["file-tool"],
            PackageId::file("file-tool", Path::new("tools/file-tool"))
        );
        assert_eq!(
            resolved.graph.dependencies[&app]["left-pad"],
            PackageId::registry("left-pad", "^1.3.0")
        );
        assert_eq!(
            resolved.graph.dependency_kind(&app, "left-pad"),
            Some(PackageDependencyKind::Runtime)
        );
        assert_eq!(resolved.graph.resolve_bin("lib").len(), 1);
        assert_eq!(resolved.graph.resolve_bin("file-tool").len(), 1);
        let lock_text = resolved.lockfile.to_toml_string().unwrap();
        assert!(lock_text.contains("[packages.\"app@workspace:.\".dependencies]"));
        assert!(lock_text.contains("left-pad = \"left-pad@npm:^1.3.0\""));
        let app_package = resolved.lockfile.packages.get("app@workspace:.").unwrap();
        assert_eq!(
            app_package.lifecycle.hooks,
            vec!["preinstall".to_string(), "postinstall".to_string()]
        );
        assert!(app_package.lifecycle.scripts.contains_key("preinstall"));
        assert!(app_package.lifecycle.scripts.contains_key("postinstall"));
        assert!(!app_package.lifecycle.scripts.contains_key("build"));
    }

    #[tokio::test]
    async fn registry_metadata_enriches_lockfile_and_populates_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("registry");
        let cache = FsRegistryMetadataCache::new(tmp.path().join("cache"));
        write(
            &tmp.path().join("project/package.json"),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0"}}"#,
        )
        .await;
        write(
            &fixture.join("left-pad.json"),
            r#"{
              "name": "left-pad",
              "dist-tags": { "latest": "1.3.0" },
              "versions": {
                "1.1.0": {
                  "name": "left-pad",
                  "version": "1.1.0",
                  "dist": {
                    "tarball": "https://registry.npmjs.org/left-pad/-/left-pad-1.1.0.tgz",
                    "integrity": "sha512-old"
                  }
                },
                "1.3.0": {
                  "name": "left-pad",
                  "version": "1.3.0",
                  "dependencies": { "repeat-string": "^1.6.1" },
                  "bin": { "left-pad": "./bin.js" },
                  "scripts": {
                    "preinstall": "node preinstall.js",
                    "build": "tsc",
                    "postinstall": "node postinstall.js"
                  },
                  "dist": {
                    "tarball": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
                    "integrity": "sha512-new"
                  }
                }
              }
            }"#,
        )
        .await;
        write(
            &fixture.join("repeat-string.json"),
            r#"{
              "name": "repeat-string",
              "dist-tags": { "latest": "1.6.1" },
              "versions": {
                "1.6.1": {
                  "name": "repeat-string",
                  "version": "1.6.1",
                  "dist": {
                    "tarball": "https://registry.npmjs.org/repeat-string/-/repeat-string-1.6.1.tgz",
                    "integrity": "sha512-repeat"
                  }
                }
              }
            }"#,
        )
        .await;

        let client = FileRegistryMetadataClient::new(&fixture);
        let mut resolution = resolve_local_project(tmp.path().join("project"))
            .await
            .unwrap();
        enrich_resolution_with_registry_metadata(
            &mut resolution,
            &cache,
            &client,
            &InstallPolicy::default(),
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        assert!(cache.metadata_path("left-pad").is_file());
        let package = resolution
            .lockfile
            .packages
            .get("left-pad@npm:1.3.0")
            .unwrap();
        assert_eq!(package.version, "1.3.0");
        assert_eq!(package.integrity.as_deref(), Some("sha512-new"));
        assert_eq!(
            package.resolved.as_ref().unwrap().reference,
            "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz"
        );
        assert_eq!(
            package.dependencies["repeat-string"],
            "repeat-string@npm:1.6.1"
        );
        assert_eq!(
            package.lifecycle.scripts["postinstall"],
            "node postinstall.js"
        );
        assert_eq!(
            package.lifecycle.hooks,
            vec!["preinstall".to_string(), "postinstall".to_string()]
        );
        assert!(!package.lifecycle.scripts.contains_key("build"));
        assert_eq!(resolution.graph.resolve_bin("left-pad").len(), 1);
    }

    #[test]
    fn manifest_lifecycle_metadata_records_implicit_node_gyp_install_hook() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("binding.gyp"), "{}\n").unwrap();
        let manifest = PackageManifest::parse_json(
            r#"{"name":"native-addon","version":"1.0.0","scripts":{"build":"tsc"}}"#,
        )
        .unwrap();

        let lifecycle = lifecycle_metadata_for_manifest(&manifest, Some(tmp.path()));

        assert_eq!(lifecycle.hooks, vec!["install".to_string()]);
        assert_eq!(
            lifecycle.scripts.get("install").map(String::as_str),
            Some("node-gyp rebuild")
        );
        assert!(!lifecycle.scripts.contains_key("build"));
    }

    #[tokio::test]
    async fn install_local_project_runs_root_lifecycle_hooks_with_project_bin_path() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        write(
            &project.join("package.json"),
            r#"{
              "name": "app",
              "version": "0.1.0",
              "scripts": {
                "preinstall": "node_modules/.bin/write-root pre",
                "postinstall": "node_modules/.bin/write-root post"
              }
            }"#,
        )
        .await;
        write(
            &project.join("node_modules/.bin/write-root"),
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> lifecycle.log\n",
        )
        .await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                project.join("node_modules/.bin/write-root"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let report = install_local_project(
            &project,
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(tmp.path().join("registry")),
            &FsPackageStore::new(tmp.path().join("package-cache")),
            &FileTarballClient::new(tmp.path().join("tarballs")),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();
        let lifecycle_log = tokio::fs::read_to_string(project.join("lifecycle.log"))
            .await
            .unwrap();

        assert_eq!(report.lifecycle_scripts, 2);
        assert_eq!(lifecycle_log, "pre\npost\n");
    }

    #[tokio::test]
    async fn local_project_resolution_records_dependency_edge_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("package.json"),
            r#"{
              "name": "app",
              "dependencies": { "runtime-dep": "^1.0.0" },
              "devDependencies": { "dev-dep": "^2.0.0" },
              "peerDependencies": { "peer-dep": "^3.0.0" },
              "optionalDependencies": { "optional-dep": "^4.0.0" }
            }"#,
        )
        .await;

        let resolved = resolve_local_project(tmp.path()).await.unwrap();
        let app = PackageId::root_workspace("app");

        assert_eq!(
            resolved.graph.dependency_kind(&app, "runtime-dep"),
            Some(PackageDependencyKind::Runtime)
        );
        assert_eq!(
            resolved.graph.dependency_kind(&app, "dev-dep"),
            Some(PackageDependencyKind::Development)
        );
        assert_eq!(
            resolved.graph.dependency_kind(&app, "peer-dep"),
            Some(PackageDependencyKind::Peer)
        );
        assert_eq!(
            resolved.graph.dependency_kind(&app, "optional-dep"),
            Some(PackageDependencyKind::Optional)
        );
    }

    #[tokio::test]
    async fn registry_metadata_records_peer_optional_edges_and_cycles() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("registry");
        write(
            &tmp.path().join("project/package.json"),
            r#"{"name":"app","dependencies":{"pkg-a":"^1.0.0"}}"#,
        )
        .await;
        write(
            &fixture.join("pkg-a.json"),
            r#"{
              "name": "pkg-a",
              "dist-tags": { "latest": "1.0.0" },
              "versions": {
                "1.0.0": {
                  "name": "pkg-a",
                  "version": "1.0.0",
                  "dependencies": { "pkg-b": "^1.0.0" },
                  "peerDependencies": { "pkg-peer": "^2.0.0" },
                  "optionalDependencies": { "pkg-optional": "^3.0.0" }
                }
              }
            }"#,
        )
        .await;
        write(
            &fixture.join("pkg-b.json"),
            r#"{
              "name": "pkg-b",
              "dist-tags": { "latest": "1.0.0" },
              "versions": {
                "1.0.0": {
                  "name": "pkg-b",
                  "version": "1.0.0",
                  "dependencies": { "pkg-a": "^1.0.0" }
                }
              }
            }"#,
        )
        .await;
        write(
            &fixture.join("pkg-peer.json"),
            r#"{
              "name": "pkg-peer",
              "dist-tags": { "latest": "2.0.0" },
              "versions": { "2.0.0": { "name": "pkg-peer", "version": "2.0.0" } }
            }"#,
        )
        .await;
        write(
            &fixture.join("pkg-optional.json"),
            r#"{
              "name": "pkg-optional",
              "dist-tags": { "latest": "3.0.0" },
              "versions": { "3.0.0": { "name": "pkg-optional", "version": "3.0.0" } }
            }"#,
        )
        .await;

        let mut resolution = resolve_local_project(tmp.path().join("project"))
            .await
            .unwrap();
        enrich_resolution_with_registry_metadata(
            &mut resolution,
            &FsRegistryMetadataCache::new(tmp.path().join("cache")),
            &FileRegistryMetadataClient::new(&fixture),
            &InstallPolicy::default(),
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        let pkg_a = PackageId::registry_version("pkg-a", "1.0.0");
        let pkg_b = PackageId::registry_version("pkg-b", "1.0.0");
        assert_eq!(resolution.graph.dependencies[&pkg_a]["pkg-b"], pkg_b);
        assert_eq!(
            resolution.graph.dependency_kind(&pkg_a, "pkg-b"),
            Some(PackageDependencyKind::Runtime)
        );
        assert_eq!(
            resolution.graph.dependency_kind(&pkg_a, "pkg-peer"),
            Some(PackageDependencyKind::Peer)
        );
        assert_eq!(
            resolution.graph.dependency_kind(&pkg_a, "pkg-optional"),
            Some(PackageDependencyKind::Optional)
        );
        assert_eq!(
            resolution.lockfile.packages[&pkg_b.to_string()].dependencies["pkg-a"],
            pkg_a.to_string()
        );
    }

    #[tokio::test]
    async fn async_registry_metadata_cache_reuses_cached_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("registry");
        let cache = FsRegistryMetadataCache::new(tmp.path().join("cache"));
        write(
            &fixture.join("left-pad.json"),
            r#"{
              "name": "left-pad",
              "dist-tags": { "latest": "1.0.0" },
              "versions": {
                "1.0.0": {
                  "name": "left-pad",
                  "version": "1.0.0"
                }
              }
            }"#,
        )
        .await;
        let client = FileRegistryMetadataClient::new(&fixture);

        let first = cache.get_or_fetch("left-pad", &client).await.unwrap();
        tokio::fs::remove_file(fixture.join("left-pad.json"))
            .await
            .unwrap();
        let second = cache.get_or_fetch("left-pad", &client).await.unwrap();

        assert_eq!(first, second);
        assert!(cache.metadata_path("left-pad").is_file());

        /// Answers every request with "not modified", recording its tag.
        struct Unchanged(std::sync::Mutex<Vec<Option<String>>>);
        impl RegistryMetadataClient for Unchanged {
            fn fetch_metadata<'a>(
                &'a self,
                _package: &'a str,
                etag: Option<&'a str>,
            ) -> Pin<Box<dyn Future<Output = Result<MetadataFetch, PackageManagerError>> + Send + 'a>>
            {
                self.0.lock().unwrap().push(etag.map(str::to_string));
                Box::pin(async { Ok(MetadataFetch::NotModified) })
            }
        }
        // A document past its freshness is asked about by its tag, and a
        // "not modified" answer renews it.
        let path = cache.metadata_path("left-pad");
        std::fs::write(tmp.path().join("cache/left-pad.etag"), "\"v1\"").unwrap();
        std::fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - METADATA_FRESH_FOR * 2)
            .unwrap();
        let unchanged = Unchanged(std::sync::Mutex::new(Vec::new()));
        let third = cache.get_or_fetch("left-pad", &unchanged).await.unwrap();
        let fourth = cache.get_or_fetch("left-pad", &unchanged).await.unwrap();
        assert_eq!(third, first);
        assert_eq!(fourth, first);
        assert_eq!(*unchanged.0.lock().unwrap(), [Some("\"v1\"".to_string())]);
    }

    #[tokio::test]
    async fn a_tarball_that_misses_its_integrity_stores_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let url = "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz";
        let fixture = tmp.path().join("tarballs");
        let tarball = npm_tgz(&[("package/index.js", "module.exports = 1;\n")]);
        write_bytes(&fixture.join(cache_key(url)), &tarball).await;
        let source = TarballSource {
            url: url.to_string(),
            integrity: Some(format!(
                "sha512-{}",
                base64::engine::general_purpose::STANDARD.encode(Sha512::digest(b"other bytes"))
            )),
        };
        let store = FsPackageStore::new(tmp.path().join("cache"));

        let refused = store
            .get_or_fetch_and_extract(&source, &FileTarballClient::new(&fixture))
            .await;

        assert!(matches!(refused, Err(PackageManagerError::Integrity { .. })));
        assert!(
            store
                .content_store()
                .read_index(&tarball::tarball_cache_key(&source))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn two_projects_share_one_copy_of_a_package_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"left-pad","version":"1.0.0"}"#,
            ),
            ("package/index.js", "module.exports = 1;\n"),
        ]);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
        );
        let url = "https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz";
        let fixture = tmp.path().join("tarballs");
        write_bytes(&fixture.join(cache_key(url)), &tarball).await;
        let mut lockfile = Lockfile::new();
        lockfile.packages.insert(
            "left-pad@npm:^1.0.0".to_string(),
            LockedPackage {
                name: "left-pad".to_string(),
                version: "1.0.0".to_string(),
                dependencies: BTreeMap::new(),
                integrity: Some(integrity),
                attested: false,
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: url.to_string(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );

        let store = FsPackageStore::new(tmp.path().join("cache"));
        let first = store
            .materialize_registry_packages(
                tmp.path().join("one"),
                &lockfile,
                &FileTarballClient::new(&fixture),
            )
            .await
            .unwrap();
        let second = store
            .materialize_registry_packages(
                tmp.path().join("two"),
                &lockfile,
                &FileTarballClient::new(&fixture),
            )
            .await
            .unwrap();

        assert!(!first[0].reused_cache);
        assert!(
            second[0].reused_cache,
            "the second project must not re-extract the archive"
        );
        let one = first[0].installed_root.join("index.js");
        let two = second[0].installed_root.join("index.js");
        assert_eq!(std::fs::read(&one).unwrap(), std::fs::read(&two).unwrap());
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use std::os::unix::fs::MetadataExt;

            let one = std::fs::metadata(&one).unwrap();
            let two = std::fs::metadata(&two).unwrap();
            assert_eq!(
                one.ino(),
                two.ino(),
                "both trees must link to the same stored bytes"
            );
        }
        // Each project clones the package's one stored tree: its own files,
        // sharing the stored blocks.
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::MetadataExt;

            assert!(tmp.path().join("cache/store/trees").read_dir().unwrap().count() == 1);
            assert_eq!(std::fs::metadata(&one).unwrap().nlink(), 1);
            assert_eq!(std::fs::metadata(&two).unwrap().nlink(), 1);
        }
    }

    #[tokio::test]
    async fn package_store_extracts_and_materializes_registry_tarballs() {
        let tmp = tempfile::tempdir().unwrap();
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"left-pad","version":"1.0.0","bin":{"left-pad":"./index.js"}}"#,
            ),
            ("package/index.js", "module.exports = 1;\n"),
        ]);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
        );
        let url = "https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz";
        let fixture = tmp.path().join("tarballs");
        write_bytes(&fixture.join(cache_key(url)), &tarball).await;
        let mut lockfile = Lockfile::new();
        lockfile.packages.insert(
            "left-pad@npm:^1.0.0".to_string(),
            LockedPackage {
                name: "left-pad".to_string(),
                version: "1.0.0".to_string(),
                dependencies: BTreeMap::new(),
                integrity: Some(integrity),
                attested: false,
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: url.to_string(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );
        let store = FsPackageStore::new(tmp.path().join("cache"));
        let first = store
            .materialize_registry_packages(
                tmp.path().join("project"),
                &lockfile,
                &FileTarballClient::new(&fixture),
            )
            .await
            .unwrap();
        let second = store
            .materialize_registry_packages(
                tmp.path().join("project"),
                &lockfile,
                &FileTarballClient::new(&fixture),
            )
            .await
            .unwrap();

        assert_eq!(first.len(), 1);
        assert!(!first[0].reused_install);
        assert_eq!(first[0].linked_bins, 1);
        assert!(second[0].reused_install);
        assert_eq!(second[0].linked_bins, 1);
        assert!(first[0].installed_root.join("index.js").is_file());
        assert!(
            tmp.path()
                .join("project/node_modules/.bin/left-pad")
                .is_file()
        );
    }

    #[tokio::test]
    async fn approved_dependency_runs_its_install_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let registry = tmp.path().join("registry");
        let tarballs = tmp.path().join("tarballs");
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"tool","version":"1.0.0","scripts":{"postinstall":"node setup.js"}}"#,
            ),
            (
                "package/setup.js",
                "require('fs').writeFileSync('postinstall-ran.txt', 'ok')\n",
            ),
        ]);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
        );
        let url = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz";
        write(
            &project.join("package.json"),
            r#"{
              "name": "app",
              "dependencies": { "tool": "^1.0.0" },
              "otter": { "install": { "allowBuilds": { "tool": true } } }
            }"#,
        )
        .await;
        write(
            &registry.join("tool.json"),
            &format!(
                r#"{{
                  "name": "tool",
                  "dist-tags": {{ "latest": "1.0.0" }},
                  "versions": {{
                    "1.0.0": {{
                      "name": "tool",
                      "version": "1.0.0",
                      "scripts": {{ "postinstall": "node setup.js" }},
                      "dist": {{
                        "tarball": "{url}",
                        "integrity": "{integrity}"
                      }}
                    }}
                  }}
                }}"#
            ),
        )
        .await;
        write_bytes(&tarballs.join(cache_key(url)), &tarball).await;
        let report = install_local_project(
            &project,
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(&registry),
            &FsPackageStore::new(tmp.path().join("package-cache")),
            &FileTarballClient::new(&tarballs),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();

        assert_eq!(report.lifecycle_scripts, 1);
        assert!(report.pending_builds.is_empty());
        assert!(
            project
                .join("node_modules/tool/postinstall-ran.txt")
                .is_file()
        );
        let lockfile = tokio::fs::read_to_string(project.join("otter.lock"))
            .await
            .unwrap();
        assert!(lockfile.contains("trust = \"trusted\""));
    }

    #[tokio::test]
    async fn refused_dependency_records_a_disabled_trust_state() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let registry = tmp.path().join("registry");
        let tarballs = tmp.path().join("tarballs");
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"tool","version":"1.0.0","scripts":{"postinstall":"node setup.js"}}"#,
            ),
            (
                "package/setup.js",
                "require('fs').writeFileSync('postinstall-ran.txt', 'ok')\n",
            ),
        ]);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
        );
        let url = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz";
        write(
            &project.join("package.json"),
            r#"{
              "name": "app",
              "dependencies": { "tool": "^1.0.0" },
              "otter": { "install": { "allowBuilds": { "tool": false } } }
            }"#,
        )
        .await;
        write(
            &registry.join("tool.json"),
            &format!(
                r#"{{
                  "name": "tool",
                  "dist-tags": {{ "latest": "1.0.0" }},
                  "versions": {{
                    "1.0.0": {{
                      "name": "tool",
                      "version": "1.0.0",
                      "scripts": {{ "postinstall": "node setup.js" }},
                      "dist": {{
                        "tarball": "{url}",
                        "integrity": "{integrity}"
                      }}
                    }}
                  }}
                }}"#
            ),
        )
        .await;
        write_bytes(&tarballs.join(cache_key(url)), &tarball).await;
        let report = install_local_project(
            &project,
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(&registry),
            &FsPackageStore::new(tmp.path().join("package-cache")),
            &FileTarballClient::new(&tarballs),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();

        assert_eq!(report.lifecycle_scripts, 0);
        assert!(
            report.pending_builds.is_empty(),
            "a reviewed refusal is not pending review"
        );
        assert!(
            !project
                .join("node_modules/tool/postinstall-ran.txt")
                .is_file()
        );
        let lockfile = tokio::fs::read_to_string(project.join("otter.lock"))
            .await
            .unwrap();
        assert!(lockfile.contains("trust = \"disabled\""));
    }

    #[tokio::test]
    async fn install_local_project_is_lockfile_stable_on_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let registry = tmp.path().join("registry");
        let tarballs = tmp.path().join("tarballs");
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"tool","version":"1.0.0","bin":{"tool":"./bin.js"},"scripts":{"postinstall":"node setup.js"}}"#,
            ),
            ("package/bin.js", "#!/usr/bin/env otter\nundefined;\n"),
            (
                "package/setup.js",
                "require('fs').writeFileSync('postinstall-ran.txt', 'ok')\n",
            ),
        ]);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
        );
        let url = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz";
        write(
            &project.join("package.json"),
            r#"{"name":"app","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await;
        write(
            &registry.join("tool.json"),
            &format!(
                r#"{{
                  "name": "tool",
                  "dist-tags": {{ "latest": "1.0.0" }},
                  "versions": {{
                    "1.0.0": {{
                      "name": "tool",
                      "version": "1.0.0",
                      "bin": {{ "tool": "./bin.js" }},
                      "scripts": {{ "postinstall": "node setup.js" }},
                      "dist": {{
                        "tarball": "{url}",
                        "integrity": "{integrity}"
                      }}
                    }}
                  }}
                }}"#
            ),
        )
        .await;
        write_bytes(&tarballs.join(cache_key(url)), &tarball).await;
        let metadata_cache = FsRegistryMetadataCache::new(tmp.path().join("metadata-cache"));
        let package_store = FsPackageStore::new(tmp.path().join("package-cache"));
        let first = install_local_project(
            &project,
            &metadata_cache,
            &FileRegistryMetadataClient::new(&registry),
            &package_store,
            &FileTarballClient::new(&tarballs),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();
        let lockfile_first = tokio::fs::read_to_string(project.join("otter.lock"))
            .await
            .unwrap();
        // The lockfile pins every version: the second install asks no
        // registry and reads no cached manifest.
        tokio::fs::remove_dir_all(&registry).await.unwrap();
        tokio::fs::remove_dir_all(tmp.path().join("metadata-cache"))
            .await
            .unwrap();
        let second = install_local_project(
            &project,
            &metadata_cache,
            &FileRegistryMetadataClient::new(&registry),
            &package_store,
            &FileTarballClient::new(&tarballs),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();
        let lockfile_second = tokio::fs::read_to_string(project.join("otter.lock"))
            .await
            .unwrap();

        assert!(first.lockfile_changed);
        assert_eq!(first.added_packages, 1);
        assert_eq!(first.imported_lockfile, None);
        assert_eq!(first.linked_bins, 1);
        assert!(!second.lockfile_changed);
        assert_eq!(second.reused_packages, 1);
        assert_eq!(second.linked_bins, 1);
        assert_eq!(lockfile_first, lockfile_second);
        assert_eq!(first.lifecycle_scripts, 0);
        assert_eq!(second.lifecycle_scripts, 0);
        assert_eq!(
            first
                .pending_builds
                .iter()
                .map(|build| build.name.as_str())
                .collect::<Vec<_>>(),
            ["tool"]
        );
        assert_eq!(first.pending_builds[0].hooks, ["postinstall"]);
        assert!(lockfile_first.contains("postinstall = \"node setup.js\""));
        assert!(
            !project
                .join("node_modules/tool/postinstall-ran.txt")
                .is_file(),
            "an unapproved dependency must not run its install scripts"
        );
        assert!(project.join("node_modules/tool/bin.js").is_file());
        assert!(project.join("node_modules/.bin/tool").is_file());
        let graph = resolve_installed_project(&project).await.unwrap().graph;
        let bins = graph.resolve_bin("tool");
        assert_eq!(bins.len(), 1);
        assert!(bins[0].path.ends_with("node_modules/.bin/tool"));
        // The project's range edge leads to the installed version, whose
        // manifest is the one on disk; no range placeholder stays behind.
        let tool = PackageId::registry_version("tool", "1.0.0");
        assert_eq!(
            graph.dependencies[&PackageId::root_workspace("app")]["tool"],
            tool
        );
        assert!(graph.packages[&tool].manifest.bin.is_some());
        assert!(!graph.packages.contains_key(&PackageId::registry("tool", "^1.0.0")));
    }

    #[tokio::test]
    async fn install_local_project_imports_package_lock_without_reresolve() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let tarballs = tmp.path().join("tarballs");
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"tool","version":"1.2.0","scripts":{"postinstall":"node setup.js"},"bin":{"tool":"./bin.js"}}"#,
            ),
            ("package/bin.js", "#!/usr/bin/env otter\nundefined;\n"),
            (
                "package/setup.js",
                "require('fs').writeFileSync('setup-ok', 'ok')\n",
            ),
        ]);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
        );
        let url = "https://registry.npmjs.org/tool/-/tool-1.2.0.tgz";
        write(
            &project.join("package.json"),
            r#"{"name":"app","version":"0.1.0","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await;
        write(
            &project.join("package-lock.json"),
            &format!(
                r#"{{
  "name": "app",
  "version": "0.1.0",
  "lockfileVersion": 3,
  "packages": {{
    "": {{
      "name": "app",
      "version": "0.1.0",
      "dependencies": {{ "tool": "^1.0.0" }}
    }},
    "node_modules/tool": {{
      "version": "1.2.0",
      "resolved": "{url}",
      "integrity": "{integrity}"
    }}
  }}
}}"#
            ),
        )
        .await;
        write_bytes(&tarballs.join(cache_key(url)), &tarball).await;

        let report = install_local_project(
            &project,
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(tmp.path().join("registry")),
            &FsPackageStore::new(tmp.path().join("package-cache")),
            &FileTarballClient::new(&tarballs),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();
        let lockfile = tokio::fs::read_to_string(project.join("otter.lock"))
            .await
            .unwrap();

        assert_eq!(
            report.imported_lockfile,
            Some(otter_pm_lockfile::LockfileFormat::PackageLock)
        );
        assert!(report.lockfile_changed);
        assert_eq!(report.added_packages, 1);
        assert!(lockfile.contains("[packages.\"tool@npm:^1.0.0\"]"));
        assert!(lockfile.contains("version = \"1.2.0\""));
        assert!(lockfile.contains("postinstall = \"node setup.js\""));
        assert!(project.join("node_modules/tool/bin.js").is_file());
        assert!(project.join("node_modules/.bin/tool").is_file());
    }

    #[tokio::test]
    async fn two_versions_of_one_name_each_reach_their_own_dependents() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let registry = tmp.path().join("registry");
        let tarballs = tmp.path().join("tarballs");
        let versions = |name: &str, entries: &[(&str, &str, &str)]| {
            entries
                .iter()
                .map(|(version, dependencies, body)| {
                    let manifest = format!(
                        r#"{{"name":"{name}","version":"{version}","dependencies":{dependencies}}}"#
                    );
                    (
                        format!("https://registry.example.test/{name}/-/{name}-{version}.tgz"),
                        npm_tgz(&[("package/package.json", &manifest), ("package/index.js", body)]),
                    )
                })
                .collect::<Vec<_>>()
        };
        let ms = versions(
            "ms",
            &[
                ("1.0.0", "{}", "module.exports = 'ms1';\n"),
                ("2.0.0", "{}", "module.exports = 'ms2';\n"),
            ],
        );
        let a = versions(
            "a",
            &[("1.0.0", r#"{"ms":"^1.0.0"}"#, "module.exports = require('ms');\n")],
        );
        let document = |name: &str, latest: &str, entries: &[(&str, &str)], files: &[(String, Vec<u8>)]| {
            let versions = entries
                .iter()
                .zip(files)
                .map(|((version, dependencies), (url, tarball))| {
                    let integrity = format!(
                        "sha512-{}",
                        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(tarball))
                    );
                    format!(
                        r#""{version}":{{"name":"{name}","version":"{version}","dependencies":{dependencies},"dist":{{"tarball":"{url}","integrity":"{integrity}"}}}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(r#"{{"name":"{name}","dist-tags":{{"latest":"{latest}"}},"versions":{{{versions}}}}}"#)
        };
        write(
            &registry.join("ms.json"),
            &document("ms", "2.0.0", &[("1.0.0", "{}"), ("2.0.0", "{}")], &ms),
        )
        .await;
        write(
            &registry.join("a.json"),
            &document("a", "1.0.0", &[("1.0.0", r#"{"ms":"^1.0.0"}"#)], &a),
        )
        .await;
        for (url, tarball) in ms.iter().chain(&a) {
            write_bytes(&tarballs.join(cache_key(url)), tarball).await;
        }
        write(
            &project.join("package.json"),
            r#"{"name":"app","dependencies":{"a":"^1.0.0","ms":"^2.0.0"}}"#,
        )
        .await;

        install_local_project(
            &project,
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(&registry),
            &FsPackageStore::new(tmp.path().join("package-cache")),
            &FileTarballClient::new(&tarballs),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();

        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();
        // The project sees the version it asked for; `a`, from where it is
        // stored, sees the one it asked for.
        assert_eq!(read(project.join("node_modules/ms/index.js")), "module.exports = 'ms2';\n");
        let stored_a = std::fs::canonicalize(project.join("node_modules/a")).unwrap();
        assert_eq!(
            read(stored_a.parent().unwrap().join("ms/index.js")),
            "module.exports = 'ms1';\n"
        );
    }

    #[tokio::test]
    async fn install_local_project_materializes_direct_tarball_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let registry = tmp.path().join("registry");
        let tarballs = tmp.path().join("tarballs");
        let url = "https://registry.example.test/tool/-/tool-1.0.0.tgz";
        let dep_url = "https://registry.example.test/dep/-/dep-1.0.0.tgz";
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"tool","version":"1.0.0","dependencies":{"dep":"^1.0.0"},"scripts":{"postinstall":"node setup.js"},"bin":{"tool":"./bin.js"}}"#,
            ),
            ("package/bin.js", "#!/usr/bin/env otter\nundefined;\n"),
            (
                "package/setup.js",
                "require('fs').writeFileSync('setup-ok', 'ok')\n",
            ),
        ]);
        let dep_tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"dep","version":"1.0.0"}"#,
            ),
            ("package/index.js", "export let value = 1;\n"),
        ]);
        let dep_integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&dep_tarball))
        );
        write(
            &project.join("package.json"),
            &format!(r#"{{"name":"app","dependencies":{{"tool":"{url}"}}}}"#),
        )
        .await;
        write(
            &registry.join("dep.json"),
            &format!(
                r#"{{
                  "name": "dep",
                  "dist-tags": {{ "latest": "1.0.0" }},
                  "versions": {{
                    "1.0.0": {{
                      "name": "dep",
                      "version": "1.0.0",
                      "dist": {{
                        "tarball": "{dep_url}",
                        "integrity": "{dep_integrity}"
                      }}
                    }}
                  }}
                }}"#
            ),
        )
        .await;
        write_bytes(&tarballs.join(cache_key(url)), &tarball).await;
        write_bytes(&tarballs.join(cache_key(dep_url)), &dep_tarball).await;

        let report = install_local_project(
            &project,
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(&registry),
            &FsPackageStore::new(tmp.path().join("package-cache")),
            &FileTarballClient::new(&tarballs),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();
        let lockfile = tokio::fs::read_to_string(project.join("otter.lock"))
            .await
            .unwrap();

        assert_eq!(report.added_packages, 2);
        assert!(lockfile.contains(
            "[packages.\"tool@tarball:https://registry.example.test/tool/-/tool-1.0.0.tgz\"]"
        ));
        assert!(lockfile.contains("version = \"1.0.0\""));
        assert!(lockfile.contains("kind = \"tarball\""));
        assert!(lockfile.contains("postinstall = \"node setup.js\""));
        assert!(lockfile.contains("dep = \"dep@npm:1.0.0\""));
        // `dep` is the tool's dependency, not the project's: it sits beside
        // the tool, and only the shared fallback names it for everyone else.
        let tool = layout::package_dir(&project, &format!("tool@tarball:{url}"), "tool");
        assert!(tool.parent().unwrap().join("dep/index.js").is_file());
        assert!(!project.join("node_modules/dep").exists());
        assert!(project.join("node_modules/.otter/node_modules/dep/index.js").is_file());
        assert!(project.join("node_modules/tool/bin.js").is_file());
        assert!(project.join("node_modules/.bin/tool").is_file());
    }

    #[tokio::test]
    async fn install_local_project_materializes_file_tarball_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"tool","version":"1.0.0","bin":{"tool":"./bin.js"}}"#,
            ),
            ("package/bin.js", "#!/usr/bin/env otter\nundefined;\n"),
        ]);
        write(
            &project.join("package.json"),
            r#"{"name":"app","dependencies":{"tool":"file:tool-1.0.0.tgz"}}"#,
        )
        .await;
        write_bytes(&project.join("tool-1.0.0.tgz"), &tarball).await;

        let report = install_local_project(
            &project,
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(tmp.path().join("registry")),
            &FsPackageStore::new(tmp.path().join("package-cache")),
            &HttpTarballClient::new(),
            &FixtureAdvisoryClient::default(),
        )
        .await
        .unwrap();
        let lockfile = tokio::fs::read_to_string(project.join("otter.lock"))
            .await
            .unwrap();

        assert_eq!(report.added_packages, 1);
        assert!(lockfile.contains("[packages.\"tool@tarball:file:tool-1.0.0.tgz\"]"));
        assert!(lockfile.contains("reference = \"file:tool-1.0.0.tgz\""));
        assert!(lockfile.contains("version = \"1.0.0\""));
        assert!(project.join("node_modules/tool/bin.js").is_file());
        assert!(project.join("node_modules/.bin/tool").is_file());
    }

    #[tokio::test]
    async fn prune_removed_registry_packages_removes_package_root_and_bins() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let stored = layout::package_dir(&project, "tool@npm:1.0.0", "tool");
        write(
            &stored.join("package.json"),
            r#"{"name":"tool","version":"1.0.0","bin":{"tool":"./bin.js"}}"#,
        )
        .await;
        write(&stored.join("bin.js"), "undefined;").await;
        write(&project.join("node_modules/.bin/tool"), "undefined;").await;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&stored, project.join("node_modules/tool")).unwrap();
        write(
            &project.join("node_modules/.otter-state/tool%40npm%3A1.0.0.source"),
            "source",
        )
        .await;
        let mut previous = Lockfile::new();
        previous.packages.insert(
            "tool@npm:1.0.0".to_string(),
            LockedPackage {
                name: "tool".to_string(),
                version: "1.0.0".to_string(),
                dependencies: BTreeMap::new(),
                integrity: Some("sha512-test".to_string()),
                attested: false,
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz".to_string(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );

        let report = prune_removed_registry_packages(&project, &previous, &Lockfile::new())
            .await
            .unwrap();

        assert_eq!(report.removed_packages, 1);
        assert_eq!(report.removed_bins, 1);
        assert!(!layout::store_entry_dir(&project, "tool@npm:1.0.0").exists());
        assert!(
            std::fs::symlink_metadata(project.join("node_modules/tool")).is_err(),
            "the project's link to a removed package goes with it"
        );
        assert!(!project.join("node_modules/.bin/tool").exists());
    }

    #[tokio::test]
    async fn installed_project_can_import_package_lock_without_otter_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        write(
            &project.join("package.json"),
            r#"{"name":"app","version":"0.1.0","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await;
        write(
            &project.join("package-lock.json"),
            r#"{
  "name": "app",
  "version": "0.1.0",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "name": "app",
      "version": "0.1.0",
      "dependencies": {
        "tool": "^1.0.0"
      }
    },
    "node_modules/tool": {
      "version": "1.2.0",
      "resolved": "https://registry.npmjs.org/tool/-/tool-1.2.0.tgz",
      "integrity": "sha512-tool"
    }
  }
}"#,
        )
        .await;
        write(
            &project.join("node_modules/tool/package.json"),
            r#"{"name":"tool","version":"1.2.0"}"#,
        )
        .await;

        let resolution = resolve_installed_project(&project).await.unwrap();
        let app = PackageId::root_workspace("app");
        let tool = PackageId::registry("tool", "^1.0.0");

        assert!(resolution.graph.package(&tool).is_some());
        assert_eq!(
            resolution
                .graph
                .dependencies
                .get(&app)
                .and_then(|deps| deps.get("tool")),
            Some(&tool)
        );
        assert_eq!(
            resolution
                .lockfile
                .packages
                .get("tool@npm:^1.0.0")
                .map(|package| package.version.as_str()),
            Some("1.2.0")
        );
    }

    #[tokio::test]
    async fn http_clients_fetch_registry_metadata_and_tarballs() {
        let metadata = r#"{
          "name": "pkg",
          "dist-tags": { "latest": "1.0.0" },
          "versions": {
            "1.0.0": {
              "name": "pkg",
              "version": "1.0.0"
            }
          }
        }"#;
        let metadata_url = serve_http_once(metadata.as_bytes().to_vec()).await;
        let metadata_client = HttpRegistryMetadataClient::with_base_url(metadata_url);
        let MetadataFetch::Fresh { metadata, .. } =
            metadata_client.fetch_metadata("pkg", None).await.unwrap()
        else {
            panic!("an unconditional request returns the document");
        };
        assert_eq!(metadata.name, "pkg");

        let tarball_url = serve_http_once(b"tgz bytes".to_vec()).await;
        let tarball = HttpTarballClient::new()
            .fetch_tarball(&tarball_url)
            .await
            .unwrap();
        assert_eq!(tarball, b"tgz bytes");
    }

    /// Project + registry fixture with one old and one freshly published
    /// version of `tool`, both satisfying `^1`.
    async fn release_age_fixture(root: &Path) {
        let fresh = chrono::Utc::now().to_rfc3339();
        write(
            &root.join("project/package.json"),
            r#"{"name":"app","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await;
        write(
            &root.join("registry/tool.json"),
            &format!(
                r#"{{
                  "name": "tool",
                  "dist-tags": {{ "latest": "1.5.0" }},
                  "versions": {{
                    "1.0.0": {{
                      "name": "tool",
                      "version": "1.0.0",
                      "dist": {{ "tarball": "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz", "integrity": "sha512-old" }}
                    }},
                    "1.5.0": {{
                      "name": "tool",
                      "version": "1.5.0",
                      "dist": {{ "tarball": "https://registry.npmjs.org/tool/-/tool-1.5.0.tgz", "integrity": "sha512-new" }}
                    }}
                  }},
                  "time": {{
                    "1.0.0": "2020-01-01T00:00:00.000Z",
                    "1.5.0": "{fresh}"
                  }}
                }}"#
            ),
        )
        .await;
    }

    #[tokio::test]
    async fn a_release_inside_the_cooling_window_falls_back_to_an_older_one() {
        let tmp = tempfile::tempdir().unwrap();
        release_age_fixture(tmp.path()).await;
        let mut warnings = Vec::new();
        let resolution = resolve_local_project_with_registry_metadata(
            tmp.path().join("project"),
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(tmp.path().join("registry")),
            &InstallPolicy::default(),
            None,
            &mut warnings,
        )
        .await
        .unwrap();

        let tool = resolution
            .lockfile
            .packages
            .values()
            .find(|package| package.name == "tool")
            .unwrap();
        assert_eq!(tool.version, "1.0.0");
        assert_eq!(
            warnings
                .iter()
                .map(|warning| warning.code)
                .collect::<Vec<_>>(),
            ["PM_RELEASE_HELD_BACK"]
        );
    }

    #[tokio::test]
    async fn a_strict_cooling_window_fails_instead_of_falling_back() {
        let tmp = tempfile::tempdir().unwrap();
        release_age_fixture(tmp.path()).await;
        let policy = InstallPolicy::from_settings(
            &serde_json::from_str(r#"{"minimumReleaseAgeStrict":true}"#).unwrap(),
        );
        let error = resolve_local_project_with_registry_metadata(
            tmp.path().join("project"),
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(tmp.path().join("registry")),
            &policy,
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(
                &error,
                PackageManagerError::ReleaseTooNew { package, version, .. }
                    if package == "tool" && version == "1.5.0"
            ),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_disabled_cooling_window_takes_the_newest_release() {
        let tmp = tempfile::tempdir().unwrap();
        release_age_fixture(tmp.path()).await;
        let policy = InstallPolicy::from_settings(
            &serde_json::from_str(r#"{"minimumReleaseAgeHours":0}"#).unwrap(),
        );
        let mut warnings = Vec::new();
        let resolution = resolve_local_project_with_registry_metadata(
            tmp.path().join("project"),
            &FsRegistryMetadataCache::new(tmp.path().join("metadata-cache")),
            &FileRegistryMetadataClient::new(tmp.path().join("registry")),
            &policy,
            None,
            &mut warnings,
        )
        .await
        .unwrap();

        let tool = resolution
            .lockfile
            .packages
            .values()
            .find(|package| package.name == "tool")
            .unwrap();
        assert_eq!(tool.version, "1.5.0");
        assert!(warnings.is_empty());
    }

    fn registry_lockfile(name: &str, version: &str, attested: bool) -> Lockfile {
        let mut lockfile = Lockfile::new();
        lockfile.packages.insert(
            format!("{name}@npm:^{version}"),
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                dependencies: BTreeMap::new(),
                integrity: Some("sha512-test".to_string()),
                attested,
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: format!("https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );
        lockfile
    }

    #[test]
    fn losing_publish_provenance_fails_resolution() {
        let previous = registry_lockfile("tool", "1.0.0", true);
        let current = registry_lockfile("tool", "1.1.0", false);
        let error =
            check_provenance_continuity(Some(&previous), &current, &InstallPolicy::default())
                .unwrap_err();
        assert!(
            matches!(
                &error,
                PackageManagerError::ProvenanceDowngrade { package, previous_version, .. }
                    if package == "tool" && previous_version == "1.0.0"
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_package_that_never_had_provenance_is_not_a_downgrade() {
        let previous = registry_lockfile("tool", "1.0.0", false);
        let current = registry_lockfile("tool", "1.1.0", false);
        check_provenance_continuity(Some(&previous), &current, &InstallPolicy::default()).unwrap();
    }

    #[test]
    fn an_off_trust_policy_admits_a_provenance_downgrade() {
        let previous = registry_lockfile("tool", "1.0.0", true);
        let current = registry_lockfile("tool", "1.1.0", false);
        let policy = InstallPolicy::from_settings(
            &serde_json::from_str(r#"{"trustPolicy":"off"}"#).unwrap(),
        );
        check_provenance_continuity(Some(&previous), &current, &policy).unwrap();
    }

    #[tokio::test]
    async fn a_known_malicious_release_fails_the_install() {
        let lockfile = registry_lockfile("tool", "1.0.0", false);
        let client = FixtureAdvisoryClient::new(vec![MaliciousAdvisory {
            name: "tool".to_string(),
            version: "1.0.0".to_string(),
            id: "MAL-2026-42".to_string(),
        }]);
        let error = check_advisories(
            &lockfile,
            None,
            &client,
            &InstallPolicy::default(),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                &error,
                PackageManagerError::MaliciousPackage { package, advisory, .. }
                    if package == "tool" && advisory == "MAL-2026-42"
            ),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_advisory_database_warns_by_default_and_fails_when_required() {
        let lockfile = registry_lockfile("tool", "1.0.0", false);
        let client = FixtureAdvisoryClient::unavailable("connection refused");

        let mut warnings = Vec::new();
        check_advisories(
            &lockfile,
            None,
            &client,
            &InstallPolicy::default(),
            &mut warnings,
        )
        .await
        .unwrap();
        assert_eq!(
            warnings
                .iter()
                .map(|warning| warning.code)
                .collect::<Vec<_>>(),
            ["PM_ADVISORY_LOOKUP_FAILED"]
        );

        let required = InstallPolicy::from_settings(
            &serde_json::from_str(r#"{"advisoryCheck":"required"}"#).unwrap(),
        );
        let error = check_advisories(&lockfile, None, &client, &required, &mut Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PackageManagerError::AdvisoryUnavailable { .. }
        ));
        // Versions the previous lockfile held were asked about then: an
        // unchanged project needs no lookup, even when one is required.
        check_advisories(
            &lockfile,
            Some(&lockfile),
            &client,
            &required,
            &mut Vec::new(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_off_advisory_check_never_queries_the_database() {
        let lockfile = registry_lockfile("tool", "1.0.0", false);
        let client = FixtureAdvisoryClient::unavailable("connection refused");
        let policy = InstallPolicy::from_settings(
            &serde_json::from_str(r#"{"advisoryCheck":"off"}"#).unwrap(),
        );
        check_advisories(&lockfile, None, &client, &policy, &mut Vec::new())
            .await
            .unwrap();
    }

    #[test]
    fn a_package_without_an_integrity_digest_warns_and_can_be_made_fatal() {
        let mut lockfile = registry_lockfile("tool", "1.0.0", false);
        for package in lockfile.packages.values_mut() {
            package.integrity = None;
        }

        let mut warnings = Vec::new();
        check_store_integrity(&lockfile, &InstallPolicy::default(), &mut warnings).unwrap();
        assert_eq!(
            warnings
                .iter()
                .map(|warning| warning.code)
                .collect::<Vec<_>>(),
            ["PM_MISSING_INTEGRITY"]
        );

        let strict = InstallPolicy::from_settings(
            &serde_json::from_str(r#"{"strictStoreIntegrity":true}"#).unwrap(),
        );
        let error = check_store_integrity(&lockfile, &strict, &mut Vec::new()).unwrap_err();
        assert!(matches!(
            error,
            PackageManagerError::MissingIntegrity { .. }
        ));
    }

    async fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, text).await.unwrap();
    }

    async fn write_bytes(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, bytes).await.unwrap();
    }

    fn npm_tgz(files: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, text) in files {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(text.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, text.as_bytes()).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    async fn serve_http_once(body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        format!("http://{addr}")
    }
}
